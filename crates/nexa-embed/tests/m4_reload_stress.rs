use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use nexa::prelude::{
    FunctionEffect, HostCallOutcome, HostFunctionAuthority, HostFunctionSlot, HostRegistry,
    HostTrap, ResolvedHostFunction, ResourceContext, RuntimeValue, ScriptArguments,
    ScriptSignature, StableId, ValueType,
};
use nexa::{
    RuntimeHostArgs, ScriptArgumentRequirements, ScriptCallError, ScriptCallWriter, ScriptExport,
    ScriptOutputReader,
};
use nexa_embed::{
    ActivationPolicy, ActivationSet, CandidateBuildContext, CandidateIdentity,
    CandidateTerminalKind, CapabilitySet, DevelopmentConfig, DevelopmentEvent, DiscoveredPackage,
    EngineDiagnosticStage, EngineError, EngineHealth, EngineTickReport, HostContract,
    MemoryPackage, MemorySource, NexaEngine, PackageContext, PackageId, PackagePolicy,
    PackageRuntimeLimits, PackageSource, PackageSourceError, ReloadReport, ReloadReportOutcome,
    SourceId, TrustLevel,
};
use serde::Serialize;

const IDL: &str = "contract TestHost {
    enum WaitError { Cancelled, }
    host {
        @cancel(return_error)
        @abandon(trap)
        async fn wait(value: i32) -> Result<i32, WaitError>;
    }
    nexa {
        fn run(value: i32) -> i32;
        async fn resource_probe(value: i32) -> i32;
    }
}";
const RUN_ID: StableId = StableId(0x8143_9374_8b64_00a6);
const RESOURCE_PROBE_ID: StableId = StableId(0xfac0_ded9_4371_1d6f);
const ITERATIONS_PER_CLASS: u64 = 100;
const ACTIVATION_RECOVERIES: u64 = 10;
const WORKER_TIMEOUT: Duration = Duration::from_secs(10);

fn host_function_authority(
    contract: &nexa::ValidatedContract,
    name: &str,
) -> HostFunctionAuthority {
    let model =
        nexa::BindingModel::from_contract(contract).expect("stress Contract runtime binding model");
    let function = model
        .host_functions
        .iter()
        .find(|function| function.identity.source_name == name)
        .expect("stress Host function is declared");
    let runtime = function
        .host_contract
        .as_ref()
        .expect("Host function has runtime metadata");
    let parameters = Box::leak(runtime.parameters.clone().into_boxed_slice());
    let capabilities: &'static [&'static str] = Box::leak(
        function
            .capabilities
            .iter()
            .cloned()
            .map(|capability| {
                let capability: &'static str = Box::leak(capability.into_boxed_str());
                capability
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    HostFunctionAuthority::new(
        function.identity.stable_id,
        function.declaration_fingerprint,
        parameters,
        runtime.result,
        runtime.mode,
        function.fuel_cost,
        runtime.async_result,
        capabilities,
    )
}

#[derive(Clone)]
struct ProjectSource {
    id: SourceId,
    policy: PackagePolicy,
    state: Arc<RwLock<ProjectState>>,
}

#[derive(Clone)]
struct NidlReloadSource {
    id: SourceId,
    policy: PackagePolicy,
    contract_source: Arc<RwLock<String>>,
    script: Arc<RwLock<String>>,
}

impl PackageSource for NidlReloadSource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn policy(&self) -> &PackagePolicy {
        &self.policy
    }

    fn discover(
        &self,
        build: &CandidateBuildContext,
    ) -> Result<Vec<DiscoveredPackage>, PackageSourceError> {
        let manifest = "schema = 2
kind = \"application\"
id = \"stress.nidl\"
name = \"M4R1 NIDL Reload Stress\"
version = \"1.0.0\"
source_root = \"src\"
entry = \"stress.nidl\"
activation = \"default-enabled\"
handler_fuel = 20000
capabilities = []
";
        let contract_source = self
            .contract_source
            .read()
            .expect("NIDL stress Contract source lock")
            .clone();
        let changed_build = CandidateBuildContext::with_source(
            nexa::SourceIdentity::standalone("contracts/m4r1-reload-stress.nidl"),
            contract_source.into_bytes(),
        )
        .requiring_entrypoints(build.required_entrypoints.clone());
        MemorySource::new(self.id.clone(), self.policy.clone())
            .package(MemoryPackage::new("stress-nidl", manifest).source(
                "src/stress/nidl.nexa",
                self.script.read().expect("NIDL stress source lock").clone(),
            ))
            .discover(&changed_build)
    }
}

#[derive(Clone)]
struct ProjectState {
    delta: i32,
    state_version: u32,
    dependency_revision: i32,
    root_override: Option<String>,
    lock_override: Option<String>,
    extra_sources: BTreeMap<String, String>,
}

impl ProjectState {
    fn root_source(&self) -> String {
        self.root_override.clone().unwrap_or_else(|| {
            format!(
                "use support::library as support;\nuse host::test_host as stress;\n\
                 @state(version = {}) class Store {{ mut value: i32, }}\n\
                 pub fn run(value: i32) -> i32 {{ return support::revision() + value + {}; }}\n",
                self.state_version, self.delta,
            )
        })
    }

    fn dependency_source(&self) -> String {
        format!(
            "pub fn revision() -> i32 {{ return {}; }}\n",
            self.dependency_revision
        )
    }
}

impl PackageSource for ProjectSource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn policy(&self) -> &PackagePolicy {
        &self.policy
    }

    fn discover(
        &self,
        build: &CandidateBuildContext,
    ) -> Result<Vec<DiscoveredPackage>, PackageSourceError> {
        let state = self.state.read().expect("stress project read lock").clone();
        let application_manifest = "schema = 2
kind = \"application\"
id = \"stress.application\"
name = \"M4 Reload Stress\"
version = \"1.0.0\"
source_root = \"src\"
entry = \"stress.app\"
activation = \"default-enabled\"
handler_fuel = 20000
capabilities = []

[dependencies]
support = { path = \"../library\" }
";
        let library_manifest = "schema = 2
kind = \"library\"
id = \"stress.library\"
name = \"M4 Reload Stress Library\"
version = \"1.0.0\"
source_root = \"src\"
";
        let lock = state.lock_override.clone().unwrap_or_else(|| {
            "schema = 1
root = \"stress.application\"

[[packages]]
id = \"stress.application\"
version = \"1.0.0\"
path = \"application\"

[[packages]]
id = \"stress.library\"
version = \"1.0.0\"
path = \"library\"

[[edges]]
from = \"stress.application\"
alias = \"support\"
to = \"stress.library\"
"
            .into()
        });
        let mut application = MemoryPackage::new("application", application_manifest)
            .source("src/stress/app.nexa", state.root_source())
            .lock(lock);
        for (path, source) in &state.extra_sources {
            application = application.source(path.clone(), source.clone());
        }
        MemorySource::new(self.id.clone(), self.policy.clone())
            .package(application)
            .package(
                MemoryPackage::new("library", library_manifest)
                    .source("src/library.nexa", state.dependency_source()),
            )
            .discover(build)
    }
}

struct Registry {
    contract_runtime_id: StableId,
    authority: HostFunctionAuthority,
    requests_created: Arc<AtomicU64>,
}

struct NidlReloadRegistry {
    contract_runtime_id: StableId,
    authority: HostFunctionAuthority,
    revision: i32,
}

impl HostRegistry for NidlReloadRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.contract_runtime_id)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        (id == self.authority.stable_id()).then_some(ResolvedHostFunction::new(
            HostFunctionSlot::new(0),
            &self.authority,
        ))
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        _: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if slot.index() != 0 {
            return Err(HostTrap::InvalidFunctionSlot(slot));
        }
        if !args.is_empty() {
            return Err(HostTrap::Arity);
        }
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::I32(
            self.revision,
        )))
    }
}

impl HostRegistry for Registry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.contract_runtime_id)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        (id == self.authority.stable_id()).then_some(ResolvedHostFunction::new(
            HostFunctionSlot::new(0),
            &self.authority,
        ))
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        context: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if slot.index() != 0 {
            return Err(HostTrap::InvalidFunctionSlot(slot));
        }
        let pending = context
            .create_request()
            .map_err(|_| HostTrap::ResourceCapacity)?;
        self.requests_created.fetch_add(1, Ordering::Relaxed);
        Ok(HostCallOutcome::Pending(pending.request))
    }
}

struct Run;

impl ScriptExport for Run {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = RUN_ID;
    const NAME: &'static str = "run";
    const CONTRACT_SLOT: usize = 1;
    const SIGNATURE: ScriptSignature =
        ScriptSignature::new(&[ValueType::I32], Some(ValueType::I32));
    const EFFECT: FunctionEffect = FunctionEffect::Ordinary;

    fn argument_requirements(
        _: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Ok(ScriptArgumentRequirements::ZERO)
    }

    fn encode_args(
        _: &mut ScriptCallWriter<'_>,
        value: &Self::Args,
    ) -> Result<ScriptArguments, ScriptCallError> {
        ScriptArguments::try_from_array([RuntimeValue::I32(*value)])
    }

    fn decode_output(
        reader: &ScriptOutputReader<'_>,
        value: RuntimeValue,
    ) -> Result<Self::Output, ScriptCallError> {
        reader
            .value(value)
            .i32()
            .map_err(|_| ScriptCallError::OutputDecoding)
    }
}

struct ResourceProbe;

impl ScriptExport for ResourceProbe {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = RESOURCE_PROBE_ID;
    const NAME: &'static str = "resource_probe";
    const CONTRACT_SLOT: usize = 0;
    const SIGNATURE: ScriptSignature = Run::SIGNATURE;
    const EFFECT: FunctionEffect = FunctionEffect::Task;

    fn argument_requirements(
        args: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Run::argument_requirements(args)
    }

    fn encode_args(
        writer: &mut ScriptCallWriter<'_>,
        args: &Self::Args,
    ) -> Result<ScriptArguments, ScriptCallError> {
        Run::encode_args(writer, args)
    }

    fn decode_output(
        reader: &ScriptOutputReader<'_>,
        value: RuntimeValue,
    ) -> Result<Self::Output, ScriptCallError> {
        Run::decode_output(reader, value)
    }
}

#[derive(Serialize)]
struct StressReport {
    schema: u32,
    status: &'static str,
    classes: BTreeMap<&'static str, u64>,
    activation_fault_recovery: u64,
    development_pipeline: BTreeMap<&'static str, u64>,
    terminal_outcomes: BTreeMap<&'static str, u64>,
    failure_evidence: BTreeMap<&'static str, u64>,
    safety: BTreeMap<&'static str, u64>,
}

#[derive(Serialize)]
struct NidlReloadTerminalReport {
    created: u64,
    terminal: u64,
    duplicate: u64,
    missing: u64,
}

#[derive(Serialize)]
struct NidlReloadSafetyReport {
    stale_candidate_committed: u64,
    active_lkg_violation: u64,
    duplicate_terminal: u64,
    missing_terminal: u64,
    task_request_resource_growth: u64,
    release_queue_not_empty: u64,
    worker_residual: u64,
}

#[derive(Serialize)]
struct NidlReloadOutcomeReport {
    nidl_rejected: u64,
    restored_committed: u64,
    host_contract_mismatch: u64,
}

#[derive(Serialize)]
struct NidlReloadStressReport {
    schema: u32,
    status: &'static str,
    iterations: u64,
    nidl_changes: u64,
    outcomes: NidlReloadOutcomeReport,
    terminal: NidlReloadTerminalReport,
    safety: NidlReloadSafetyReport,
    failures: Vec<String>,
}

#[derive(Default)]
struct StressTrace {
    events: Vec<DevelopmentEvent>,
    reloads: Vec<ReloadReport>,
    stale_identities: Vec<CandidateIdentity>,
    late_results_observed: u64,
    active_lkg_violations: u64,
}

impl StressTrace {
    fn record(&mut self, report: EngineTickReport) {
        self.events.extend(report.development_events);
        self.reloads.extend(report.reloads);
    }

    fn mark_stale(&mut self, identity: CandidateIdentity) {
        self.stale_identities.push(identity);
    }
}

fn nidl_reload_script(host_function: &str, delta: i32) -> String {
    format!(
        "use host::test_host as host;\n\
         pub fn run(value: i32) -> i32 {{\n\
             return host::{host_function}() + value + {delta};\n\
         }}\n"
    )
}

fn transient_resource_growth(before: EngineHealth, after: EngineHealth) -> bool {
    after.tasks > before.tasks
        || after.scopes > before.scopes
        || after.continuations > before.continuations
        || after.scheduler_tokens > before.scheduler_tokens
        || after.requests > before.requests
        || after.completion_reservations > before.completion_reservations
        || after.tokens > before.tokens
        || after.snapshots > before.snapshots
        || after.release_reservations > before.release_reservations
        || after.heap_objects > before.heap_objects
        || after.state_objects > before.state_objects
        || after.retired_modules > before.retired_modules
        || after.host_pending_completions > before.host_pending_completions
        || after.host_pending_releases > before.host_pending_releases
}

#[test]
#[ignore = "M4R1 machine-evidence NIDL reload stress gate"]
#[allow(clippy::too_many_lines)]
fn m4r1_nidl_reload_stress() {
    const ITERATIONS: u64 = 100;
    const REVISION: i32 = 50;
    const NAME_ALPHABET: &[u8; 26] = b"abcdefghijklmnopqrstuvwxyz";
    const BASE_CONTRACT: &str = "contract TestHost {
        host {
            fn revision() -> i32;
        }
        nexa {
            fn run(value: i32) -> i32;
        }
    }";

    let mut contract_fingerprints = BTreeSet::new();
    let mut stale_candidate_committed = 0_u64;
    let mut active_lkg_violation = 0_u64;
    let mut task_request_resource_growth = 0_u64;
    let mut release_queue_not_empty = 0_u64;
    let mut contract_mismatch_reason_missing = 0_u64;
    let mut changed_reload_outcome_mismatch = 0_u64;
    let mut restored_reload_outcome_mismatch = 0_u64;
    let mut unchanged_candidate_fingerprint = 0_u64;
    let mut nidl_rejected = 0_u64;
    let mut restored_committed = 0_u64;
    let mut host_contract_mismatch = 0_u64;
    let base_contract = nexa::parse_nidl(BASE_CONTRACT).expect("base NIDL stress Contract");
    let run = base_contract
        .nexa_functions
        .iter()
        .find(|function| function.name == Run::NAME)
        .expect("base NIDL stress Contract declares run");
    assert_eq!(nexa::entrypoint_stable_id(run), RUN_ID);
    let descriptor = nexa::abi_descriptor(&base_contract);
    let fingerprint = descriptor.fingerprint.into_bytes();
    let descriptor: &'static [u8] = Box::leak(descriptor.bytes.into_boxed_slice());
    let contract_runtime_id = nexa::contract_runtime_id(&base_contract);
    let authority = host_function_authority(&base_contract, "revision");
    let contract = HostContract::new(
        "TestHost",
        BASE_CONTRACT,
        descriptor,
        fingerprint,
        contract_runtime_id,
        nexa::HOST_CONTRACT_SCHEMA_VERSION,
    );
    let contract_source = Arc::new(RwLock::new(BASE_CONTRACT.to_owned()));
    let script = Arc::new(RwLock::new(nidl_reload_script("revision", 1)));
    let source = NidlReloadSource {
        id: SourceId::new("m4r1-nidl-reload").expect("NIDL stress Source ID"),
        policy: PackagePolicy {
            trust: TrustLevel::FirstParty,
            capability_ceiling: CapabilitySet::default(),
            allowed_activation: ActivationSet::new([ActivationPolicy::DefaultEnabled]),
            max_packages: 1,
            runtime_limits: PackageRuntimeLimits::default(),
            allow_entitlement: false,
        },
        contract_source: Arc::clone(&contract_source),
        script: Arc::clone(&script),
    };
    let mut engine = NexaEngine::builder(contract)
        .host_contract_source(
            nexa::SourceIdentity::standalone("contracts/m4r1-reload-stress.nidl"),
            BASE_CONTRACT,
        )
        .host_factory(move |_: &PackageContext| {
            Box::new(NidlReloadRegistry {
                contract_runtime_id,
                authority: authority.clone(),
                revision: REVISION,
            }) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<Run>()
        .build()
        .expect("build NIDL reload stress Engine");
    engine.discover().expect("discover NIDL stress Package");
    let package_id = PackageId::new("stress.nidl").expect("NIDL stress Package ID");
    engine
        .enable(&package_id)
        .expect("enable NIDL stress Package");
    assert_eq!(
        engine
            .call::<Run>(&package_id, &7)
            .expect("initial NIDL stress call")
            .value,
        REVISION + 8
    );
    engine.tick().expect("settle initial NIDL stress call");
    let baseline_health = engine.health();
    let baseline_development = engine.inspection().development;
    let mut active_delta = 1_i32;

    for iteration in 0..ITERATIONS {
        let high = usize::try_from(iteration / 26).expect("stress iteration fits usize");
        let low = usize::try_from(iteration % 26).expect("stress iteration fits usize");
        let host_function = format!(
            "revision_{}{}",
            char::from(NAME_ALPHABET[high]),
            char::from(NAME_ALPHABET[low])
        );
        let changed_contract_source = format!(
            "contract TestHost {{\n\
                 host {{\n\
                     fn {host_function}() -> i32;\n\
                 }}\n\
                 nexa {{\n\
                     fn run(value: i32) -> i32;\n\
                 }}\n\
             }}\n"
        );
        let changed_contract =
            nexa::parse_nidl(&changed_contract_source).expect("changed NIDL stress Contract");
        assert!(
            contract_fingerprints.insert(nexa::contract_fingerprint(&changed_contract)),
            "iteration {iteration} reused a Contract ABI fingerprint"
        );
        let before_identity = engine
            .inspection()
            .packages
            .iter()
            .find(|package| package.package_id == package_id)
            .and_then(|package| package.active_identity.clone())
            .expect("active identity before changed Contract candidate");
        let before_fingerprint = before_identity.build_fingerprint;

        *contract_source
            .write()
            .expect("NIDL stress Contract source lock") = changed_contract_source;
        *script.write().expect("NIDL stress source lock") =
            nidl_reload_script(&host_function, active_delta.saturating_add(10_000));
        let changed_result = engine.reload(&package_id);
        nidl_rejected += u64::from(changed_result.is_err());
        let precise_host_contract_mismatch = matches!(
            &changed_result,
            Err(EngineError::Diagnostic(diagnostic))
                if diagnostic.stage == EngineDiagnosticStage::Compile
                    && diagnostic.diagnostic.code == nexa::ErrorCode::NX7001
                    && diagnostic.diagnostic.message.to_string()
                        == "Host contract does not match resolved build input"
        );
        host_contract_mismatch += u64::from(precise_host_contract_mismatch);
        contract_mismatch_reason_missing += u64::from(!precise_host_contract_mismatch);
        stale_candidate_committed += u64::from(changed_result.is_ok());
        let rejected_inspection = engine.inspection();
        changed_reload_outcome_mismatch += u64::from(
            rejected_inspection
                .recent_reloads
                .last()
                .map(|reload| reload.outcome)
                != Some(ReloadReportOutcome::CompileFailed),
        );
        let rejected_package = rejected_inspection
            .packages
            .iter()
            .find(|package| package.package_id == package_id)
            .expect("Package inspection after changed Contract candidate");
        let rejected_identity = rejected_package
            .active_identity
            .clone()
            .expect("active identity after changed Contract candidate");
        stale_candidate_committed += u64::from(rejected_identity != before_identity);
        unchanged_candidate_fingerprint +=
            u64::from(rejected_package.desired_build_fingerprint == Some(before_fingerprint));
        let lkg_value = engine
            .call::<Run>(&package_id, &7)
            .ok()
            .map(|output| output.value);
        active_lkg_violation += u64::from(lkg_value != Some(REVISION + 7 + active_delta));

        active_delta = i32::try_from(iteration)
            .expect("NIDL stress iteration fits i32")
            .saturating_add(2);
        *contract_source
            .write()
            .expect("NIDL stress Contract source lock") = BASE_CONTRACT.to_owned();
        *script.write().expect("NIDL stress source lock") =
            nidl_reload_script("revision", active_delta);
        let restored_result = engine.reload(&package_id);
        let restored_inspection = engine.inspection();
        let restore_has_committed_outcome = restored_inspection
            .recent_reloads
            .last()
            .is_some_and(|reload| reload.outcome == ReloadReportOutcome::Committed);
        restored_committed += u64::from(restored_result.is_ok() && restore_has_committed_outcome);
        restored_reload_outcome_mismatch +=
            u64::from(restored_result.is_err() || !restore_has_committed_outcome);
        let active_value = engine
            .call::<Run>(&package_id, &7)
            .ok()
            .map(|output| output.value);
        active_lkg_violation += u64::from(active_value != Some(REVISION + 7 + active_delta));
        for _ in 0..4 {
            engine.tick().expect("settle NIDL stress reload");
        }

        let health = engine.health();
        task_request_resource_growth +=
            u64::from(transient_resource_growth(baseline_health, health));
        release_queue_not_empty += u64::from(
            health.queued_releases != 0
                || health.host_pending_releases != 0
                || health.host_pending_completions != 0,
        );
    }

    let nidl_changes =
        u64::try_from(contract_fingerprints.len()).expect("NIDL stress fingerprint count fits u64");
    let inspection = engine.inspection();
    let created = inspection
        .development
        .created_generations
        .saturating_sub(baseline_development.created_generations);
    let terminal = inspection
        .development
        .terminal_generations
        .saturating_sub(baseline_development.terminal_generations);
    let duplicate = inspection
        .development
        .duplicate_terminals
        .saturating_sub(baseline_development.duplicate_terminals);
    let missing = inspection.development.generations_without_terminal;
    engine.shutdown().expect("shutdown NIDL stress Engine");
    let development = engine.inspection().development;
    let worker_residual = u64::from(
        development.worker_running
            || development.queued_candidates != 0
            || development.generations_without_terminal != 0
            || development.worker.queued_packages != 0
            || development.worker.in_flight_package.is_some()
            || development.worker.completed_results != 0,
    );
    let safety = NidlReloadSafetyReport {
        stale_candidate_committed,
        active_lkg_violation,
        duplicate_terminal: duplicate,
        missing_terminal: missing,
        task_request_resource_growth,
        release_queue_not_empty,
        worker_residual,
    };
    let mut failures = Vec::new();
    let expected_terminals = ITERATIONS.saturating_mul(2);
    if created != expected_terminals || terminal != expected_terminals {
        failures.push(format!(
            "terminal accounting mismatch: expected={expected_terminals}, created={created}, terminal={terminal}"
        ));
    }
    if nidl_changes != ITERATIONS {
        failures.push(format!(
            "NIDL changes were not unique: expected={ITERATIONS}, actual={nidl_changes}"
        ));
    }
    if contract_mismatch_reason_missing != 0 {
        failures.push(format!(
            "{contract_mismatch_reason_missing} changed Contracts lacked precise mismatch evidence"
        ));
    }
    if changed_reload_outcome_mismatch != 0 {
        failures.push(format!(
            "{changed_reload_outcome_mismatch} changed Contracts lacked CompileFailed reload outcomes"
        ));
    }
    if restored_reload_outcome_mismatch != 0 {
        failures.push(format!(
            "{restored_reload_outcome_mismatch} restored Contracts lacked Committed reload outcomes"
        ));
    }
    if nidl_rejected != ITERATIONS
        || restored_committed != ITERATIONS
        || host_contract_mismatch != ITERATIONS
    {
        failures.push(format!(
            "outcome accounting mismatch: nidl_rejected={nidl_rejected}, restored_committed={restored_committed}, host_contract_mismatch={host_contract_mismatch}"
        ));
    }
    if unchanged_candidate_fingerprint != 0 {
        failures.push(format!(
            "{unchanged_candidate_fingerprint} NIDL changes did not change the candidate fingerprint"
        ));
    }
    if safety.stale_candidate_committed != 0
        || safety.active_lkg_violation != 0
        || safety.duplicate_terminal != 0
        || safety.missing_terminal != 0
        || safety.task_request_resource_growth != 0
        || safety.release_queue_not_empty != 0
        || safety.worker_residual != 0
    {
        failures.push("one or more NIDL reload safety counters were non-zero".into());
    }
    let status = if failures.is_empty() { "PASS" } else { "FAIL" };
    let report = NidlReloadStressReport {
        schema: 1,
        status,
        iterations: ITERATIONS,
        nidl_changes,
        outcomes: NidlReloadOutcomeReport {
            nidl_rejected,
            restored_committed,
            host_contract_mismatch,
        },
        terminal: NidlReloadTerminalReport {
            created,
            terminal,
            duplicate,
            missing,
        },
        safety,
        failures,
    };
    let report_json =
        serde_json::to_string_pretty(&report).expect("serialize M4R1 NIDL reload report");
    if let Some(path) = std::env::var_os("NEXA_M4R1_NIDL_RELOAD_STRESS_REPORT") {
        let path = PathBuf::from(path);
        std::fs::create_dir_all(path.parent().expect("NIDL reload report parent"))
            .expect("create NIDL reload report directory");
        std::fs::write(path, format!("{report_json}\n")).expect("write M4R1 NIDL reload report");
    }
    println!("{report_json}");
    assert_eq!(report.status, "PASS", "{:#?}", report.failures);
}

#[test]
#[ignore = "M4 machine-evidence stress gate"]
#[allow(clippy::too_many_lines)]
fn m4_reload_stress() {
    let idl = nexa::parse_nidl(IDL).expect("stress Host contract");
    let run = idl
        .nexa_functions
        .iter()
        .find(|function| function.name == Run::NAME)
        .expect("stress Host Contract declares run");
    assert_eq!(nexa::entrypoint_stable_id(run), RUN_ID);
    let resource_probe = idl
        .nexa_functions
        .iter()
        .find(|function| function.name == ResourceProbe::NAME)
        .expect("stress Host Contract declares resource_probe");
    assert_eq!(
        nexa::entrypoint_stable_id(resource_probe),
        RESOURCE_PROBE_ID
    );
    assert!(resource_probe.is_async);
    let descriptor = nexa::abi_descriptor(&idl);
    let fingerprint = descriptor.fingerprint.into_bytes();
    let descriptor: &'static [u8] = Box::leak(descriptor.bytes.into_boxed_slice());
    let contract = HostContract::new(
        "TestHost",
        IDL,
        descriptor,
        fingerprint,
        nexa::contract_runtime_id(&idl),
        nexa::HOST_CONTRACT_SCHEMA_VERSION,
    );
    let contract_runtime_id = contract.contract_runtime_id();
    let authority = host_function_authority(&idl, "wait");
    let state = Arc::new(RwLock::new(ProjectState {
        delta: 0,
        state_version: 1,
        dependency_revision: 0,
        root_override: None,
        lock_override: None,
        extra_sources: BTreeMap::new(),
    }));
    let source = ProjectSource {
        id: SourceId::new("m4-reload-stress").expect("stress source ID"),
        policy: PackagePolicy {
            trust: TrustLevel::FirstParty,
            capability_ceiling: CapabilitySet::default(),
            allowed_activation: ActivationSet::new([ActivationPolicy::DefaultEnabled]),
            max_packages: 2,
            runtime_limits: PackageRuntimeLimits::default(),
            allow_entitlement: false,
        },
        state: Arc::clone(&state),
    };
    let requests_created = Arc::new(AtomicU64::new(0));
    let registry_requests = Arc::clone(&requests_created);
    let mut engine = NexaEngine::builder(contract)
        .host_factory(move |_: &PackageContext| {
            Box::new(Registry {
                contract_runtime_id,
                authority: authority.clone(),
                requests_created: Arc::clone(&registry_requests),
            }) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<Run>()
        .development(DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            retain_events: 32_768,
            ..DevelopmentConfig::default()
        })
        .build()
        .expect("build stress Engine");
    let discovered = engine.discover().expect("discover stress project");
    assert_eq!(discovered.len(), 1, "only application roots enter Engine");
    assert_eq!(
        discovered[0].id.as_str(),
        "stress.application",
        "the linked library must not become an independently toggleable Realm"
    );
    assert_eq!(
        engine.status(&PackageId::new("stress.library").expect("library Package ID")),
        None,
        "a linked library must not appear in Engine PackageInspection/status"
    );
    let package_id = PackageId::new("stress.application").expect("stress Package ID");
    engine
        .enable(&package_id)
        .expect("enable stress application through the linked package artifact");
    engine.tick().expect("settle initial resources");

    state
        .write()
        .expect("stress project write lock")
        .root_override = Some(
        "use support::library as support;\nuse host::test_host as stress;\n\
         @state(version = 1) class Store { mut value: i32, }\n\
         pub fn run(value: i32) -> i32 { return support::revision() + value; }\n\
         pub async fn resource_probe(value: i32) -> i32 {\n\
             let result: Result<i32, stress::WaitError> = stress::wait(value).await;\n\
             return match result { Result::Ok(found) => found, Result::Err(error) => 0 };\n\
         }\n"
        .into(),
    );
    engine
        .reload(&package_id)
        .expect("load the Host request resource probe");
    let probe_result = engine
        .call_optional::<ResourceProbe>(&package_id, &7)
        .expect("the probe Candidate implements its optional entrypoint");
    assert!(matches!(probe_result, Err(EngineError::Handler(_, _))));
    assert_eq!(
        engine
            .diagnostics()
            .last()
            .expect("Host wait diagnostic")
            .diagnostic
            .code,
        nexa::ErrorCode::NX7102
    );
    assert_eq!(
        requests_created.load(Ordering::Relaxed),
        1,
        "the Host request resource probe never created a real request"
    );
    state
        .write()
        .expect("stress project write lock")
        .root_override = None;
    engine
        .reload(&package_id)
        .expect("recover after the Host request resource probe");
    engine.tick().expect("settle resource probe releases");
    assert_active_behavior(&mut engine, &state, &package_id);
    let baseline_health = engine.health();
    assert_transient_resources_released(baseline_health, "Host request resource probe cleanup");
    let baseline = resources(baseline_health);

    let before_lock_drift = active_identity(&engine, &package_id);
    state
        .write()
        .expect("stress project write lock")
        .lock_override = Some(
        "schema = 1
root = \"stress.application\"

[[packages]]
id = \"stress.application\"
version = \"1.0.0\"
path = \"application\"

[[packages]]
id = \"stress.library\"
version = \"1.0.0\"
path = \"library\"
"
        .into(),
    );
    assert!(
        engine.reload(&package_id).is_err(),
        "dependency Lock drift was accepted"
    );
    state
        .write()
        .expect("stress project write lock")
        .lock_override = None;
    assert!(
        lkg_preserved(&mut engine, &state, &package_id, &before_lock_drift),
        "dependency Lock drift changed the Active/LKG Candidate"
    );

    let mut successful_reload = 0_u64;
    let mut syntax_failure = 0_u64;
    let mut type_failure = 0_u64;
    let mut verifier_failure = 0_u64;
    let mut migration_failure = 0_u64;
    let mut dependency_change = 0_u64;
    let mut aba_change = 0_u64;
    let mut add_module = 0_u64;
    let mut delete_module = 0_u64;
    let mut rename_module = 0_u64;
    let mut activation_fault_recovery = 0_u64;
    let mut trace = StressTrace::default();
    let stress_generation_baseline = package_inspection(&engine, &package_id).candidate_generation;
    let stress_terminal_baseline = package_inspection(&engine, &package_id).terminal_generations;

    for _ in 0..ITERATIONS_PER_CLASS {
        state.write().expect("stress project write lock").delta += 1;
        let identity = queue_late_result(&mut engine, &mut trace, &package_id, "successful Reload");
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &identity,
            ReloadReportOutcome::Committed,
        );
        assert_eq!(active_identity(&engine, &package_id), identity);
        assert_active_behavior(&mut engine, &state, &package_id);
        successful_reload += 1;
    }

    for _ in 0..ITERATIONS_PER_CLASS {
        state
            .write()
            .expect("stress project write lock")
            .dependency_revision += 1;
        let identity = queue_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            "dependency-only Reload",
        );
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &identity,
            ReloadReportOutcome::Committed,
        );
        assert_eq!(active_identity(&engine, &package_id), identity);
        assert_active_behavior(&mut engine, &state, &package_id);
        dependency_change += 1;
    }

    for iteration in 0..ITERATIONS_PER_CLASS {
        let before = active_identity(&engine, &package_id);
        state
            .write()
            .expect("stress project write lock")
            .root_override = Some(format!("# invalid_{iteration}\n"));
        let identity = queue_late_result(&mut engine, &mut trace, &package_id, "syntax failure");
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &identity,
            ReloadReportOutcome::CompileFailed,
        );
        let matching_events = trace
            .events
            .iter()
            .filter(|event| event.data().identity == identity)
            .collect::<Vec<_>>();
        let matching_evidence = matching_events
            .iter()
            .map(|event| {
                (
                    event.kind(),
                    event.data().diagnostic.as_ref().map(|diagnostic| {
                        (
                            diagnostic.stage,
                            diagnostic.diagnostic.code.as_str().to_owned(),
                        )
                    }),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            matching_events.iter().any(|event| {
                matches!(event, DevelopmentEvent::CompileFailed(data)
                if data.diagnostic.as_ref().is_some_and(|diagnostic| {
                    diagnostic.stage == EngineDiagnosticStage::Parse
                }))
            }),
            "syntax failure did not retain a Parse-stage primary diagnostic: \
             {matching_evidence:?}"
        );
        syntax_failure += 1;
        trace.active_lkg_violations += u64::from(!active_lkg_preserved(
            &mut engine,
            &state,
            &package_id,
            &before,
        ));
    }
    state
        .write()
        .expect("stress project write lock")
        .root_override = None;
    tick_traced(&mut engine, &mut trace, "restore after syntax failures");
    assert!(active_matches_source(&engine, &state, &package_id));

    for iteration in 0..ITERATIONS_PER_CLASS {
        let before = active_identity(&engine, &package_id);
        state
            .write()
            .expect("stress project write lock")
            .root_override = Some(format!(
            "use support::library as support;\nuse host::test_host as stress;\n\
                 @state(version = 1) class Store {{ mut value: i32, }}\n\
                 pub fn run(value: i32) -> i32 {{ return support::revision() + missing_{iteration}; }}\n"
        ));
        let identity = queue_late_result(&mut engine, &mut trace, &package_id, "type failure");
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &identity,
            ReloadReportOutcome::CompileFailed,
        );
        assert!(trace.events.iter().any(|event| {
            matches!(event, DevelopmentEvent::CompileFailed(data)
            if data.identity == identity
                && data.diagnostic.as_ref().is_some_and(|diagnostic| {
                    diagnostic.stage == EngineDiagnosticStage::TypeCheck
                }))
        }));
        type_failure += 1;
        trace.active_lkg_violations += u64::from(!active_lkg_preserved(
            &mut engine,
            &state,
            &package_id,
            &before,
        ));
    }
    state
        .write()
        .expect("stress project write lock")
        .root_override = None;
    tick_traced(&mut engine, &mut trace, "restore after type failures");
    assert!(active_matches_source(&engine, &state, &package_id));

    for iteration in 0..ITERATIONS_PER_CLASS {
        let before = active_identity(&engine, &package_id);
        let bound = 1_025_u64.saturating_add(iteration);
        state
            .write()
            .expect("stress project write lock")
            .root_override = Some(format!(
            "use support::library as support;\nuse host::test_host as stress;\n\
                 @state(version = 1) class Store {{ mut value: i32, }}\n\
                 @immediate\n\
                 pub fn run(value: i32) -> i32 {{\n\
                     for step in 0..{bound} {{\n\
                         if step == {} {{ return value; }}\n\
                     }}\n\
                     return support::revision() + value;\n\
                 }}\n",
            bound.saturating_sub(1)
        ));
        let identity = queue_late_result(&mut engine, &mut trace, &package_id, "verifier failure");
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &identity,
            ReloadReportOutcome::VerifyFailed,
        );
        assert!(trace.events.iter().any(|event| {
            matches!(event, DevelopmentEvent::VerifyFailed(data)
            if data.identity == identity
                && data.diagnostic.as_ref().is_some_and(|diagnostic| {
                    diagnostic.stage == EngineDiagnosticStage::Verify
                        && diagnostic.diagnostic.code == nexa::ErrorCode::NX3001
                }))
        }));
        verifier_failure += 1;
        trace.active_lkg_violations += u64::from(!active_lkg_preserved(
            &mut engine,
            &state,
            &package_id,
            &before,
        ));
    }
    state
        .write()
        .expect("stress project write lock")
        .root_override = None;
    tick_traced(&mut engine, &mut trace, "restore after verifier failures");
    assert!(active_matches_source(&engine, &state, &package_id));

    for iteration in 0..ITERATIONS_PER_CLASS {
        let before = active_identity(&engine, &package_id);
        state
            .write()
            .expect("stress project write lock")
            .state_version =
            u32::try_from(iteration.saturating_add(2)).expect("stress state version");
        let identity = queue_late_result(&mut engine, &mut trace, &package_id, "migration failure");
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &identity,
            ReloadReportOutcome::RolledBackBeforeCommit,
        );
        assert!(trace.events.iter().any(|event| {
            matches!(event, DevelopmentEvent::ReloadRolledBack(data)
            if data.identity == identity
                && data.reload.as_ref().is_some_and(|reload| {
                    reload.outcome == ReloadReportOutcome::RolledBackBeforeCommit
                }))
        }));
        let diagnostic = package_inspection(&engine, &package_id)
            .recent_diagnostic
            .expect("migration rollback diagnostic");
        assert_eq!(diagnostic.stage, EngineDiagnosticStage::Reload);
        assert!(
            diagnostic
                .message
                .to_ascii_lowercase()
                .contains("migration"),
            "migration rollback was not backed by a migration-specific failure: {}",
            diagnostic.message
        );
        migration_failure += 1;
        trace.active_lkg_violations += u64::from(!active_lkg_preserved(
            &mut engine,
            &state,
            &package_id,
            &before,
        ));
    }
    state
        .write()
        .expect("stress project write lock")
        .state_version = 1;
    tick_traced(&mut engine, &mut trace, "restore after migration failures");
    assert!(active_matches_source(&engine, &state, &package_id));

    for _ in 0..ITERATIONS_PER_CLASS {
        state.write().expect("stress project write lock").delta += 1;
        let stale_add =
            queue_late_result(&mut engine, &mut trace, &package_id, "pre-add stale result");
        trace.mark_stale(stale_add.clone());
        state
            .write()
            .expect("stress project write lock")
            .extra_sources
            .insert(
                "src/stress/extra.nexa".into(),
                "pub fn marker() -> i32 { return 1; }\n".into(),
            );
        let added = queue_late_result_after_stale(
            &mut engine,
            &mut trace,
            &package_id,
            &stale_add,
            "source-add Reload",
        );
        assert_stale_rejected(&trace, &stale_add);
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &added,
            ReloadReportOutcome::Committed,
        );
        assert_eq!(active_identity(&engine, &package_id), added);
        assert_active_behavior(&mut engine, &state, &package_id);
        add_module += 1;

        state.write().expect("stress project write lock").delta += 1;
        let stale_rename = queue_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            "pre-rename stale result",
        );
        trace.mark_stale(stale_rename.clone());
        let mut project = state.write().expect("stress project write lock");
        let renamed_source = project
            .extra_sources
            .remove("src/stress/extra.nexa")
            .expect("source to rename");
        project
            .extra_sources
            .insert("src/stress/renamed.nexa".into(), renamed_source);
        drop(project);
        let renamed = queue_late_result_after_stale(
            &mut engine,
            &mut trace,
            &package_id,
            &stale_rename,
            "source-rename Reload",
        );
        assert_stale_rejected(&trace, &stale_rename);
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &renamed,
            ReloadReportOutcome::Committed,
        );
        assert_eq!(active_identity(&engine, &package_id), renamed);
        assert_active_behavior(&mut engine, &state, &package_id);
        rename_module += 1;

        state.write().expect("stress project write lock").delta += 1;
        let stale_delete = queue_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            "pre-delete stale result",
        );
        trace.mark_stale(stale_delete.clone());
        state
            .write()
            .expect("stress project write lock")
            .extra_sources
            .remove("src/stress/renamed.nexa");
        let deleted = queue_late_result_after_stale(
            &mut engine,
            &mut trace,
            &package_id,
            &stale_delete,
            "source-delete Reload",
        );
        assert_stale_rejected(&trace, &stale_delete);
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &deleted,
            ReloadReportOutcome::Committed,
        );
        assert_eq!(active_identity(&engine, &package_id), deleted);
        delete_module += 1;
        assert_active_behavior(&mut engine, &state, &package_id);
    }

    for iteration in 0..ITERATIONS_PER_CLASS {
        let before = active_identity(&engine, &package_id);
        let original_delta = state.read().expect("stress project read lock").delta;
        state.write().expect("stress project write lock").delta = original_delta
            .saturating_add(10_000)
            .saturating_add(i32::try_from(iteration).expect("stress ABA iteration fits i32"));
        let stale_aba =
            queue_late_result(&mut engine, &mut trace, &package_id, "ABA B late result");
        trace.mark_stale(stale_aba.clone());
        state.write().expect("stress project write lock").delta = original_delta;
        tick_traced(&mut engine, &mut trace, "ABA A freshness refresh");
        assert_stale_rejected(&trace, &stale_aba);
        aba_change += 1;
        let after = active_identity(&engine, &package_id);
        trace.active_lkg_violations +=
            u64::from(after != before || !active_matches_source(&engine, &state, &package_id));
    }

    for iteration in 0..ACTIVATION_RECOVERIES {
        let before = active_identity(&engine, &package_id);
        let before_value = engine
            .call::<Run>(&package_id, &1)
            .expect("call Active/LKG before activation-fault Candidate")
            .value;
        state
            .write()
            .expect("stress project write lock")
            .root_override = Some(format!(
            "use support::library as support;\nuse host::test_host as stress;\n\
                 @state(version = 1) class Store {{ mut value: i32, }}\n\
                 pub fn run(value: i32) -> i32 {{ return support::revision() + value; }}\n\
                 @activation pub fn activate() -> i32 {{\n\
                     let marker: i32 = {};\n\
                     let zero: i32 = 0;\n\
                     return marker / zero;\n\
                 }}\n",
            iteration.saturating_add(1)
        ));
        let identity =
            queue_late_result(&mut engine, &mut trace, &package_id, "activation failure");
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &identity,
            ReloadReportOutcome::ActivationFaulted,
        );
        let activation_lkg_preserved = active_identity(&engine, &package_id) == before
            && engine
                .call::<Run>(&package_id, &1)
                .is_ok_and(|output| output.value == before_value);
        trace.active_lkg_violations += u64::from(!activation_lkg_preserved);
        state
            .write()
            .expect("stress project write lock")
            .root_override = None;
        state.write().expect("stress project write lock").delta += 1;
        let recovery = queue_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            "activation-fault recovery",
        );
        drain_late_result(
            &mut engine,
            &mut trace,
            &package_id,
            &recovery,
            ReloadReportOutcome::Committed,
        );
        activation_fault_recovery += 1;
        trace.active_lkg_violations += u64::from(
            active_identity(&engine, &package_id) != recovery
                || !active_matches_source(&engine, &state, &package_id),
        );
        assert_active_behavior(&mut engine, &state, &package_id);
        assert_ne!(active_identity(&engine, &package_id), before);
    }

    for _ in 0..4 {
        tick_traced(&mut engine, &mut trace, "drain final retired resources");
    }
    let idle_inspection = engine.inspection();
    let idle_worker = &idle_inspection.development.worker;
    let idle_worker_residual = idle_inspection.development.queued_candidates != 0
        || idle_worker.queued_packages != 0
        || idle_worker.in_flight_package.is_some()
        || idle_worker.completed_results != 0;
    let task_request_resource_growth = u64::from(
        resources(idle_inspection.health)
            .into_iter()
            .zip(baseline)
            .any(|(current, initial)| current > initial),
    );
    let idle_release_queue_not_empty = idle_inspection.health.queued_releases != 0
        || idle_inspection.health.host_pending_releases != 0
        || idle_inspection.health.release_reservations != 0;
    engine.shutdown().expect("clean stress Engine shutdown");
    let inspection = engine.inspection();
    let package = inspection
        .packages
        .iter()
        .find(|package| package.package_id == package_id)
        .expect("stress Package inspection");
    let stress_generation_count = package
        .candidate_generation
        .checked_sub(stress_generation_baseline)
        .expect("stress generation counter is monotonic");
    let stress_terminal_count = package
        .terminal_generations
        .checked_sub(stress_terminal_baseline)
        .expect("stress terminal counter is monotonic");
    assert_eq!(
        stress_generation_count, stress_terminal_count,
        "every Development generation must have exactly one terminal"
    );
    let stale_candidate_committed = u64::try_from(
        trace
            .stale_identities
            .iter()
            .filter(|stale| {
                trace.reloads.iter().any(|reload| {
                    &reload.identity == *stale && reload.outcome == ReloadReportOutcome::Committed
                })
            })
            .count(),
    )
    .expect("stale commit count");
    assert!(
        trace
            .stale_identities
            .iter()
            .all(|identity| stale_was_rejected(&trace, identity)),
        "every deliberately stale late Result must have a superseded event and terminal report"
    );
    let release_queue_not_empty = u64::from(
        idle_release_queue_not_empty
            || inspection.health.queued_releases != 0
            || inspection.health.host_pending_releases != 0
            || inspection.health.release_reservations != 0,
    );
    let worker = &inspection.development.worker;
    let worker_residual = u64::from(
        idle_worker_residual
            || inspection.development.worker_running
            || inspection.development.queued_candidates != 0
            || worker.queued_packages != 0
            || worker.in_flight_package.is_some()
            || worker.completed_results != 0,
    );
    let development_pipeline = BTreeMap::from([
        (
            "change_detected",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::ChangeDetected(_))
            }),
        ),
        (
            "compile_queued",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::CompileQueued(_))
            }),
        ),
        (
            "compile_started",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::CompileStarted(_))
            }),
        ),
        ("late_results_observed", trace.late_results_observed),
        (
            "host_requests_exercised",
            requests_created.load(Ordering::Relaxed),
        ),
        (
            "candidate_superseded",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::CandidateSuperseded(_))
            }),
        ),
        (
            "reload_committed",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::ReloadCommitted(_))
            }),
        ),
    ]);
    let terminal_outcomes = BTreeMap::from([
        (
            "committed",
            reload_count(&trace, ReloadReportOutcome::Committed),
        ),
        (
            "compile_failed",
            reload_count(&trace, ReloadReportOutcome::CompileFailed),
        ),
        (
            "verify_failed",
            reload_count(&trace, ReloadReportOutcome::VerifyFailed),
        ),
        (
            "rolled_back_before_commit",
            reload_count(&trace, ReloadReportOutcome::RolledBackBeforeCommit),
        ),
        (
            "activation_faulted",
            reload_count(&trace, ReloadReportOutcome::ActivationFaulted),
        ),
        (
            "superseded",
            reload_count(&trace, ReloadReportOutcome::Superseded),
        ),
        (
            "host_rebuild_required",
            reload_count(&trace, ReloadReportOutcome::HostRebuildRequired),
        ),
    ]);
    let failure_evidence = BTreeMap::from([
        (
            "syntax_parse_diagnostic",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::CompileFailed(data)
                if data.diagnostic.as_ref().is_some_and(|diagnostic| {
                    diagnostic.stage == EngineDiagnosticStage::Parse
                }))
            }),
        ),
        (
            "typecheck_diagnostic",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::CompileFailed(data)
                if data.diagnostic.as_ref().is_some_and(|diagnostic| {
                    diagnostic.stage == EngineDiagnosticStage::TypeCheck
                }))
            }),
        ),
        (
            "verifier_diagnostic",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::VerifyFailed(data)
                if data.diagnostic.as_ref().is_some_and(|diagnostic| {
                    diagnostic.stage == EngineDiagnosticStage::Verify
                }))
            }),
        ),
        (
            "migration_rollback",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::ReloadRolledBack(_))
            }),
        ),
        (
            "activation_fault",
            event_count(&trace, |event| {
                matches!(event, DevelopmentEvent::ActivationFaulted(_))
            }),
        ),
    ]);
    assert_eq!(
        development_pipeline["change_detected"],
        stress_generation_count
    );
    assert_eq!(
        development_pipeline["compile_queued"],
        stress_generation_count
    );
    assert_eq!(
        development_pipeline["compile_started"],
        stress_generation_count
    );
    assert_eq!(
        development_pipeline["late_results_observed"],
        stress_generation_count
    );
    assert!(
        development_pipeline["host_requests_exercised"] > 0,
        "resource-growth safety must be backed by a real Host request lifecycle"
    );
    assert_eq!(
        development_pipeline["candidate_superseded"],
        u64::try_from(trace.stale_identities.len()).expect("stale identity count")
    );
    assert_eq!(
        development_pipeline["reload_committed"],
        successful_reload
            .saturating_add(dependency_change)
            .saturating_add(add_module)
            .saturating_add(rename_module)
            .saturating_add(delete_module)
            .saturating_add(activation_fault_recovery),
        "every successful/recovery class count must come from a real ReloadCommitted event"
    );
    assert_eq!(
        terminal_outcomes["committed"],
        development_pipeline["reload_committed"]
    );
    assert_eq!(
        terminal_outcomes["compile_failed"],
        syntax_failure.saturating_add(type_failure)
    );
    assert_eq!(terminal_outcomes["verify_failed"], verifier_failure);
    assert_eq!(
        terminal_outcomes["rolled_back_before_commit"],
        migration_failure
    );
    assert_eq!(
        terminal_outcomes["activation_faulted"],
        activation_fault_recovery
    );
    assert_eq!(
        terminal_outcomes["superseded"],
        add_module
            .saturating_add(delete_module)
            .saturating_add(rename_module)
            .saturating_add(aba_change)
    );
    assert_eq!(terminal_outcomes["host_rebuild_required"], 0);
    assert_eq!(
        terminal_outcomes.values().copied().sum::<u64>(),
        stress_terminal_count,
        "terminal outcome evidence must account for every Development generation"
    );
    assert_eq!(failure_evidence["syntax_parse_diagnostic"], syntax_failure);
    assert_eq!(failure_evidence["typecheck_diagnostic"], type_failure);
    assert_eq!(failure_evidence["verifier_diagnostic"], verifier_failure);
    assert_eq!(failure_evidence["migration_rollback"], migration_failure);
    assert_eq!(
        failure_evidence["activation_fault"],
        activation_fault_recovery
    );
    let report = StressReport {
        schema: 2,
        status: "PASS",
        classes: BTreeMap::from([
            ("successful_reload", successful_reload),
            ("syntax_failure", syntax_failure),
            ("type_failure", type_failure),
            ("verifier_failure", verifier_failure),
            ("migration_failure", migration_failure),
            ("dependency_change", dependency_change),
            ("aba_change", aba_change),
            ("add_module", add_module),
            ("delete_module", delete_module),
            ("rename_module", rename_module),
        ]),
        activation_fault_recovery,
        development_pipeline,
        terminal_outcomes,
        failure_evidence,
        safety: BTreeMap::from([
            ("stale_candidate_committed", stale_candidate_committed),
            ("active_lkg_violation", trace.active_lkg_violations),
            (
                "duplicate_terminal",
                inspection.development.duplicate_terminals,
            ),
            (
                "missing_terminal",
                inspection.development.generations_without_terminal,
            ),
            ("task_request_resource_growth", task_request_resource_growth),
            ("release_queue_not_empty", release_queue_not_empty),
            ("worker_residual", worker_residual),
        ]),
    };
    assert!(
        report
            .classes
            .values()
            .all(|count| *count >= ITERATIONS_PER_CLASS)
    );
    assert!(report.activation_fault_recovery >= ACTIVATION_RECOVERIES);
    assert!(
        report.safety.values().all(|count| *count == 0),
        "non-zero safety counters: {:#?}",
        report.safety
    );

    let json = serde_json::to_string_pretty(&report).expect("serialize M4 Reload stress report");
    if let Some(path) = std::env::var_os("NEXA_M4_RELOAD_STRESS_REPORT") {
        let path = PathBuf::from(path);
        std::fs::create_dir_all(path.parent().expect("stress report parent"))
            .expect("create stress report directory");
        std::fs::write(path, format!("{json}\n")).expect("write M4 Reload stress report");
    }
    println!("{json}");
}

fn queue_late_result(
    engine: &mut NexaEngine,
    trace: &mut StressTrace,
    package_id: &PackageId,
    label: &str,
) -> CandidateIdentity {
    queue_late_result_impl(engine, trace, package_id, None, label)
}

fn queue_late_result_after_stale(
    engine: &mut NexaEngine,
    trace: &mut StressTrace,
    package_id: &PackageId,
    stale: &CandidateIdentity,
    label: &str,
) -> CandidateIdentity {
    queue_late_result_impl(engine, trace, package_id, Some(stale), label)
}

fn queue_late_result_impl(
    engine: &mut NexaEngine,
    trace: &mut StressTrace,
    package_id: &PackageId,
    stale: Option<&CandidateIdentity>,
    label: &str,
) -> CandidateIdentity {
    let before = package_inspection(engine, package_id);
    let completed_before = engine.inspection().development.worker.completed_results;
    if stale.is_some() {
        assert!(
            completed_before > 0,
            "{label}: the deliberately stale late Result was not queued"
        );
    } else {
        assert_eq!(
            completed_before, 0,
            "{label}: the previous worker Result was not drained"
        );
    }
    let report = engine
        .tick()
        .unwrap_or_else(|error| panic!("{label}: Development discovery tick failed: {error}"));
    let identity = report
        .development_events
        .iter()
        .find_map(|event| match event {
            DevelopmentEvent::ChangeDetected(data)
                if data.identity.package_id == *package_id
                    && data.identity.generation
                        == before.candidate_generation.saturating_add(1) =>
            {
                Some(data.identity.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "{label}: source mutation did not create Development generation {}",
                before.candidate_generation.saturating_add(1)
            )
        });
    assert!(
        report.development_events.iter().any(|event| {
            matches!(event, DevelopmentEvent::CompileQueued(data)
                if data.identity == identity)
        }),
        "{label}: generation {} never entered the Development compile queue",
        identity.generation
    );
    assert!(
        report
            .reloads
            .iter()
            .all(|reload| reload.identity != identity),
        "{label}: generation {} completed before the late-Result observation window",
        identity.generation
    );
    trace.record(report);
    if let Some(stale) = stale {
        assert_stale_rejected(trace, stale);
    }
    wait_for_worker_result(engine, label);
    let inspection = engine.inspection();
    assert!(
        inspection.development.worker.completed_results > 0,
        "{label}: completed Result was not retained in the worker queue"
    );
    assert_eq!(
        package_inspection(engine, package_id).candidate_generation,
        identity.generation,
        "{label}: unexpected generation was created while no Engine tick ran"
    );
    trace.late_results_observed = trace.late_results_observed.saturating_add(1);
    identity
}

fn drain_late_result(
    engine: &mut NexaEngine,
    trace: &mut StressTrace,
    package_id: &PackageId,
    identity: &CandidateIdentity,
    expected_outcome: ReloadReportOutcome,
) {
    let before = package_inspection(engine, package_id);
    assert!(
        engine.inspection().development.worker.completed_results > 0,
        "generation {} has no queued late Result to drain",
        identity.generation
    );
    let report = engine.tick().unwrap_or_else(|error| {
        panic!(
            "drain generation {} ({expected_outcome:?}) failed: {error}",
            identity.generation
        )
    });
    assert!(
        report
            .reloads
            .iter()
            .any(|reload| { reload.identity == *identity && reload.outcome == expected_outcome }),
        "generation {} did not produce expected terminal report {expected_outcome:?}: {:?}",
        identity.generation,
        report
            .reloads
            .iter()
            .map(|reload| (&reload.identity, reload.outcome))
            .collect::<Vec<_>>()
    );
    trace.record(report);
    let after = package_inspection(engine, package_id);
    assert_eq!(
        after.terminal_generations,
        before.terminal_generations.saturating_add(1),
        "generation {} did not receive exactly one terminal",
        identity.generation
    );
    assert_eq!(
        after.latest_terminal_generation,
        Some(identity.generation),
        "generation {} was not the latest terminal",
        identity.generation
    );
    let expected_terminal = match expected_outcome {
        ReloadReportOutcome::Committed
        | ReloadReportOutcome::RolledBackBeforeCommit
        | ReloadReportOutcome::ActivationFaulted => CandidateTerminalKind::Compiled,
        ReloadReportOutcome::CompileFailed => CandidateTerminalKind::CompileFailed,
        ReloadReportOutcome::VerifyFailed => CandidateTerminalKind::VerifyFailed,
        ReloadReportOutcome::Superseded | ReloadReportOutcome::HostRebuildRequired => {
            panic!("drain_late_result does not accept {expected_outcome:?}")
        }
    };
    assert_eq!(
        after.latest_terminal_kind,
        Some(expected_terminal),
        "generation {} received the wrong terminal kind",
        identity.generation
    );
    assert_eq!(
        engine.inspection().development.worker.completed_results,
        0,
        "generation {} left a worker Result behind",
        identity.generation
    );
}

fn tick_traced(engine: &mut NexaEngine, trace: &mut StressTrace, label: &str) {
    let report = engine
        .tick()
        .unwrap_or_else(|error| panic!("{label}: Engine tick failed: {error}"));
    trace.record(report);
}

fn wait_for_worker_result(engine: &NexaEngine, label: &str) {
    let deadline = Instant::now() + WORKER_TIMEOUT;
    while Instant::now() < deadline {
        if engine.inspection().development.worker.completed_results > 0 {
            return;
        }
        std::thread::yield_now();
    }
    panic!("{label}: Development worker did not publish a completed Result");
}

fn package_inspection(
    engine: &NexaEngine,
    package_id: &PackageId,
) -> nexa_embed::PackageInspection {
    engine
        .inspection()
        .packages
        .into_iter()
        .find(|package| package.package_id == *package_id)
        .expect("stress Package inspection")
}

fn assert_stale_rejected(trace: &StressTrace, identity: &CandidateIdentity) {
    assert!(
        stale_was_rejected(trace, identity),
        "stale generation {} was not rejected by commit-time freshness",
        identity.generation
    );
}

fn stale_was_rejected(trace: &StressTrace, identity: &CandidateIdentity) -> bool {
    trace.events.iter().any(|event| {
        matches!(event, DevelopmentEvent::CandidateSuperseded(data)
            if data.identity == *identity)
    }) && trace.reloads.iter().any(|reload| {
        reload.identity == *identity && reload.outcome == ReloadReportOutcome::Superseded
    })
}

fn event_count(trace: &StressTrace, predicate: impl Fn(&DevelopmentEvent) -> bool) -> u64 {
    u64::try_from(trace.events.iter().filter(|event| predicate(event)).count())
        .expect("Development event count")
}

fn reload_count(trace: &StressTrace, outcome: ReloadReportOutcome) -> u64 {
    u64::try_from(
        trace
            .reloads
            .iter()
            .filter(|reload| reload.outcome == outcome)
            .count(),
    )
    .expect("Reload outcome count")
}

fn active_identity(engine: &NexaEngine, package_id: &PackageId) -> nexa_embed::CandidateIdentity {
    engine
        .inspection()
        .packages
        .iter()
        .find(|package| package.package_id == *package_id)
        .and_then(|package| package.active_identity.clone())
        .expect("active stress Candidate identity")
}

fn active_matches_source(
    engine: &NexaEngine,
    state: &Arc<RwLock<ProjectState>>,
    package_id: &PackageId,
) -> bool {
    let source = ProjectSource {
        id: SourceId::new("m4-reload-stress").expect("stress source ID"),
        policy: PackagePolicy {
            trust: TrustLevel::FirstParty,
            capability_ceiling: CapabilitySet::default(),
            allowed_activation: ActivationSet::new([ActivationPolicy::DefaultEnabled]),
            max_packages: 2,
            runtime_limits: PackageRuntimeLimits::default(),
            allow_entitlement: false,
        },
        state: Arc::clone(state),
    };
    let desired = source
        .discover(
            &CandidateBuildContext::new(IDL.as_bytes().to_vec()).requiring_entrypoints([Run::NAME]),
        )
        .expect("rediscover current stress Candidate")
        .into_iter()
        .find(|candidate| candidate.manifest.id == *package_id)
        .expect("current stress Candidate")
        .build_fingerprint;
    active_identity(engine, package_id).build_fingerprint == desired
}

fn lkg_preserved(
    engine: &mut NexaEngine,
    state: &Arc<RwLock<ProjectState>>,
    package_id: &PackageId,
    before: &nexa_embed::CandidateIdentity,
) -> bool {
    active_lkg_preserved(engine, state, package_id, before)
        && active_matches_source(engine, state, package_id)
}

fn active_lkg_preserved(
    engine: &mut NexaEngine,
    state: &Arc<RwLock<ProjectState>>,
    package_id: &PackageId,
    before: &nexa_embed::CandidateIdentity,
) -> bool {
    let state_snapshot = state.read().expect("stress project read lock");
    let expected = 1 + state_snapshot.delta + state_snapshot.dependency_revision;
    drop(state_snapshot);
    &active_identity(engine, package_id) == before
        && engine
            .call::<Run>(package_id, &1)
            .is_ok_and(|output| output.value == expected)
}

fn assert_active_behavior(
    engine: &mut NexaEngine,
    state: &Arc<RwLock<ProjectState>>,
    package_id: &PackageId,
) {
    let state_snapshot = state.read().expect("stress project read lock");
    let expected = 1 + state_snapshot.delta + state_snapshot.dependency_revision;
    drop(state_snapshot);
    assert_eq!(
        engine
            .call::<Run>(package_id, &1)
            .expect("call current stress Candidate")
            .value,
        expected,
        "active behavior did not match the root and linked dependency sources"
    );
}

fn resources(health: EngineHealth) -> [u64; 15] {
    [
        health.tasks,
        health.scopes,
        health.continuations,
        health.scheduler_tokens,
        health.requests,
        health.completion_reservations,
        health.tokens,
        health.snapshots,
        health.release_reservations,
        health.queued_releases,
        health.heap_objects,
        health.state_objects,
        health.retired_modules,
        u64::try_from(health.host_pending_completions).unwrap_or(u64::MAX),
        u64::try_from(health.host_pending_releases).unwrap_or(u64::MAX),
    ]
}

fn assert_transient_resources_released(health: EngineHealth, label: &str) {
    assert_eq!(
        health.scopes, 1,
        "{label} did not retain exactly the enabled Realm's root scope"
    );
    let residual = [
        ("tasks", health.tasks),
        ("continuations", health.continuations),
        ("scheduler_tokens", health.scheduler_tokens),
        ("requests", health.requests),
        ("completion_reservations", health.completion_reservations),
        ("tokens", health.tokens),
        ("snapshots", health.snapshots),
        ("release_reservations", health.release_reservations),
        ("queued_releases", health.queued_releases),
        ("retired_modules", health.retired_modules),
        (
            "host_pending_completions",
            u64::try_from(health.host_pending_completions).unwrap_or(u64::MAX),
        ),
        (
            "host_pending_releases",
            u64::try_from(health.host_pending_releases).unwrap_or(u64::MAX),
        ),
    ]
    .into_iter()
    .filter(|(_, count)| *count != 0)
    .map(|(name, count)| format!("{name}={count}"))
    .collect::<Vec<_>>();
    assert!(
        residual.is_empty(),
        "{label} left transient resources behind: {}",
        residual.join(", ")
    );
}
