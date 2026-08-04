use super::*;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use nexa::prelude::{
    FunctionEffect, HostCallOutcome, HostFunctionAuthority, HostFunctionSlot, HostRegistry,
    HostTrap, ResolvedHostFunction, ResourceContext, RuntimeHostArgs, RuntimeValue,
    ScriptArgumentRequirements, ScriptArguments, ScriptCallError, ScriptCallWriter, ScriptExport,
    ScriptOutputReader, ScriptSignature, StableId, ValueType,
};
use serde::Serialize;

const IDL_SOURCE: &str = "contract TestHost {
    enum WaitError { Cancelled, }
    host {
        @cancel(return_error)
        @abandon(trap)
        async fn wait(value: i32) -> Result<i32, WaitError>;
    }
    nexa {
        fn run(value: i32) -> i32;
    }
}";
const RUN_ID: StableId = StableId(0x8143_9374_8b64_00a6);
const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const INPUT: i32 = 10;
const ACTIVE_A: i32 = 11;
const STALE_B: i32 = 12;
const ACTIVE_C: i32 = 13;
static HOST_AUTHORITY: OnceLock<HostFunctionAuthority> = OnceLock::new();

struct Registry {
    contract_runtime_id: StableId,
    authority: HostFunctionAuthority,
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
        _: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::InvalidFunctionSlot(slot))
    }
}

fn host_function_authority() -> HostFunctionAuthority {
    HOST_AUTHORITY
        .get_or_init(|| {
            let contract = nexa::parse_nidl(IDL_SOURCE).expect("freshness test NIDL");
            let model = nexa::BindingModel::from_contract(&contract)
                .expect("freshness Contract runtime binding model");
            let function = model
                .host_functions
                .iter()
                .find(|function| function.identity.source_name == "wait")
                .expect("freshness Host wait function");
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
        })
        .clone()
}

struct Run;

impl ScriptExport for Run {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = RUN_ID;
    const NAME: &'static str = "run";
    const CONTRACT_SLOT: usize = 0;
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
        args: &Self::Args,
    ) -> Result<ScriptArguments, ScriptCallError> {
        ScriptArguments::try_from_array([RuntimeValue::I32(*args)])
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

#[derive(Clone)]
struct SharedSource {
    id: SourceId,
    policy: PackagePolicy,
    manifest: String,
    script: Arc<RwLock<String>>,
    available: Arc<RwLock<bool>>,
}

impl SharedSource {
    fn replace(&self, source: impl Into<String>) {
        *self.script.write().expect("freshness source lock") = source.into();
    }

    fn current_build_fingerprint(&self) -> BuildFingerprint {
        let candidate = self
            .discover(
                &CandidateBuildContext::new(IDL_SOURCE.as_bytes().to_vec())
                    .requiring_entrypoints([Run::NAME]),
            )
            .expect("freshness source discovery")
            .remove(0);
        candidate.candidate.build_fingerprint
    }

    fn set_available(&self, available: bool) {
        *self.available.write().expect("freshness availability lock") = available;
    }
}

impl PackageSource for SharedSource {
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
        if !*self.available.read().expect("freshness availability lock") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "freshness source is unavailable",
            )
            .into());
        }
        let script = self.script.read().expect("freshness source lock").clone();
        let module = self
            .manifest
            .lines()
            .find_map(|line| line.trim().strip_prefix("entry = \""))
            .and_then(|value| value.strip_suffix('"'))
            .expect("schema-2 freshness entry");
        MemorySource::new(self.id.clone(), self.policy.clone())
            .package(
                MemoryPackage::new(self.id.as_str(), self.manifest.clone())
                    .source(format!("src/{}.nexa", module.replace('.', "/")), script),
            )
            .discover(build)
    }
}

#[derive(Clone)]
struct SwitchAfterDiscoverySource {
    inner: SharedSource,
    replacement: Arc<RwLock<Option<String>>>,
}

impl SwitchAfterDiscoverySource {
    fn arm(&self, replacement: String) {
        *self
            .replacement
            .write()
            .expect("switching source replacement lock") = Some(replacement);
    }
}

impl PackageSource for SwitchAfterDiscoverySource {
    fn id(&self) -> &SourceId {
        self.inner.id()
    }

    fn policy(&self) -> &PackagePolicy {
        self.inner.policy()
    }

    fn discover(
        &self,
        build: &CandidateBuildContext,
    ) -> Result<Vec<DiscoveredPackage>, PackageSourceError> {
        let discovered = self.inner.discover(build);
        if let Some(replacement) = self
            .replacement
            .write()
            .expect("switching source replacement lock")
            .take()
        {
            self.inner.replace(replacement);
        }
        discovered
    }
}

#[derive(Clone)]
struct DynamicApplicationsSource {
    id: SourceId,
    policy: PackagePolicy,
    applications: Arc<RwLock<BTreeMap<PackageId, i32>>>,
}

impl DynamicApplicationsSource {
    fn insert(&self, package_id: PackageId, delta: i32) {
        self.applications
            .write()
            .expect("dynamic applications lock")
            .insert(package_id, delta);
    }

    fn remove(&self, package_id: &PackageId) {
        self.applications
            .write()
            .expect("dynamic applications lock")
            .remove(package_id);
    }
}

impl PackageSource for DynamicApplicationsSource {
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
        let applications = self
            .applications
            .read()
            .expect("dynamic applications lock")
            .clone();
        let mut source = MemorySource::new(self.id.clone(), self.policy.clone());
        for (package_id, delta) in applications {
            source = source.package(
                MemoryPackage::new(
                    package_id.as_str().replace('.', "-"),
                    format!(
                        "schema = 2\n\
                         kind = \"application\"\n\
                         id = \"{package_id}\"\n\
                         name = \"Dynamic {package_id}\"\n\
                         version = \"1.0.0\"\n\
                         source_root = \"src\"\n\
                         entry = \"{package_id}\"\n\
                         activation = \"default-enabled\"\n\
                         handler_fuel = 20000\n\
                         capabilities = []\n"
                    ),
                )
                .source(
                    format!("src/{}.nexa", package_id.as_str().replace('.', "/")),
                    program(delta),
                ),
            );
        }
        source.discover(build)
    }
}

struct Fixture {
    engine: NexaEngine,
    target: SharedSource,
    target_id: PackageId,
    initial_source: String,
    initial_hash: BuildFingerprint,
    blocker: Option<SharedSource>,
    blocker_id: Option<PackageId>,
}

impl Fixture {
    fn new(label: &str, config: DevelopmentConfig, with_blocker: bool) -> Self {
        let target_id = PackageId::new(format!("tests.freshness{label}")).expect("target ID");
        let initial_source = program(1);
        let target = shared_source(
            &format!("freshness-{label}"),
            &target_id,
            initial_source.clone(),
        );
        let initial_hash = target.current_build_fingerprint();
        let contract = contract();
        let contract_runtime_id = contract.contract_runtime_id();
        let authority = host_function_authority();
        let mut builder = NexaEngine::builder(contract)
            .host_factory(move |_: &PackageContext| {
                Box::new(Registry {
                    contract_runtime_id,
                    authority: authority.clone(),
                }) as Box<dyn HostRegistry>
            })
            .require_export::<Run>()
            .development(config);

        let (blocker, blocker_id) = if with_blocker {
            let blocker_id =
                PackageId::new(format!("tests.freshness{label}blocker")).expect("blocker ID");
            let blocker = shared_source(
                &format!("freshness-{label}-blocker"),
                &blocker_id,
                program(1),
            );
            builder = builder.package_source(blocker.clone());
            (Some(blocker), Some(blocker_id))
        } else {
            (None, None)
        };

        let mut engine = builder
            .package_source(target.clone())
            .build()
            .expect("build freshness Engine");
        engine.discover().expect("discover freshness Packages");
        engine.enable_defaults().expect("enable freshness Packages");
        assert_eq!(
            engine
                .call::<Run>(&target_id, &INPUT)
                .expect("initial active Runtime")
                .value,
            ACTIVE_A
        );

        Self {
            engine,
            target,
            target_id,
            initial_source,
            initial_hash,
            blocker,
            blocker_id,
        }
    }

    fn set_target_delta(&self, delta: i32) -> BuildFingerprint {
        self.target.replace(program(delta));
        self.target.current_build_fingerprint()
    }

    fn restore_target(&self) {
        self.target.replace(self.initial_source.clone());
    }

    fn change_blocker(&self) {
        let blocker = self.blocker.as_ref().expect("fixture has a blocker");
        blocker.replace(program(9));
    }
}

#[derive(Default)]
struct ScenarioTrace {
    events: Vec<DevelopmentEvent>,
    reloads: Vec<ReloadReport>,
    active_runtime_violations: u64,
    last_known_good_violations: u64,
}

impl ScenarioTrace {
    fn tick_checked(
        &mut self,
        engine: &mut NexaEngine,
        package_id: &PackageId,
        stale_hash: BuildFingerprint,
        stale_value: i32,
    ) {
        let report = engine.tick().expect("freshness Engine tick");
        self.events.extend(report.development_events);
        self.reloads.extend(report.reloads);
        self.observe_runtime(engine, package_id, stale_hash, stale_value);
    }

    fn observe_runtime(
        &mut self,
        engine: &mut NexaEngine,
        package_id: &PackageId,
        stale_hash: BuildFingerprint,
        stale_value: i32,
    ) {
        let value = engine
            .call::<Run>(package_id, &INPUT)
            .expect("freshness Runtime remains callable")
            .value;
        if value == stale_value {
            self.active_runtime_violations = self.active_runtime_violations.saturating_add(1);
        }
        let active_hash = package_inspection(engine, package_id).build_fingerprint;
        if active_hash == stale_hash {
            self.last_known_good_violations = self.last_known_good_violations.saturating_add(1);
        }
        assert_ne!(value, stale_value, "a stale Candidate became active");
        assert_ne!(
            active_hash, stale_hash,
            "a stale Candidate replaced Last Known Good"
        );
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScenarioEvidence {
    name: String,
    stage: String,
    stale_candidates_observed: u64,
    stale_candidates_committed: u64,
    desired_build_fingerprint_mismatches_rejected: u64,
    superseded_before_compile: u64,
    superseded_after_compile: u64,
    created_generations: u64,
    terminal_generations: u64,
    duplicate_terminals: u64,
    generations_without_terminal: u64,
    active_runtime_violations: u64,
    last_known_good_violations: u64,
    status: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FreshnessReport {
    schema: u32,
    scenario_count: u64,
    pending_scenario_count: u64,
    in_flight_scenario_count: u64,
    result_queue_scenario_count: u64,
    ready_candidate_scenario_count: u64,
    stale_candidates_observed: u64,
    stale_candidates_committed: u64,
    desired_build_fingerprint_mismatches_rejected: u64,
    superseded_before_compile: u64,
    superseded_after_compile: u64,
    created_generations: u64,
    terminal_generations: u64,
    duplicate_terminals: u64,
    generations_without_terminal: u64,
    active_runtime_violations: u64,
    last_known_good_violations: u64,
    scenarios: Vec<ScenarioEvidence>,
    status: String,
}

fn contract() -> HostContract {
    static CONTRACT: OnceLock<HostContract> = OnceLock::new();
    *CONTRACT.get_or_init(|| {
        let contract = nexa::parse_nidl(IDL_SOURCE).expect("freshness test IDL");
        let run = contract
            .nexa_functions
            .iter()
            .find(|entrypoint| entrypoint.name == Run::NAME)
            .expect("freshness required entrypoint is declared by the Contract");
        assert_eq!(nexa::entrypoint_stable_id(run), Run::STABLE_ID);
        let descriptor = nexa::abi_descriptor(&contract);
        let fingerprint = descriptor.fingerprint.into_bytes();
        let descriptor: &'static [u8] = Box::leak(descriptor.bytes.into_boxed_slice());
        HostContract::new(
            "TestHost",
            IDL_SOURCE,
            descriptor,
            fingerprint,
            nexa::contract_runtime_id(&contract),
            nexa::HOST_CONTRACT_SCHEMA_VERSION,
        )
    })
}

fn policy() -> PackagePolicy {
    PackagePolicy {
        trust: TrustLevel::Trusted,
        capability_ceiling: CapabilitySet::default(),
        allowed_activation: ActivationSet::new([ActivationPolicy::DefaultEnabled]),
        max_packages: 4,
        runtime_limits: PackageRuntimeLimits::default(),
        allow_entitlement: false,
    }
}

fn shared_source(id: &str, package_id: &PackageId, script: String) -> SharedSource {
    SharedSource {
        id: SourceId::new(id).expect("freshness Source ID"),
        policy: policy(),
        manifest: format!(
            "schema = 2\n\
             kind = \"application\"\n\
             id = \"{package_id}\"\n\
             name = \"Freshness\"\n\
             version = \"1.0.0\"\n\
             source_root = \"src\"\n\
             entry = \"{package_id}\"\n\
             activation = \"default-enabled\"\n\
             handler_fuel = 20000\n\
             capabilities = []\n"
        ),
        script: Arc::new(RwLock::new(script)),
        available: Arc::new(RwLock::new(true)),
    }
}

fn program(delta: i32) -> String {
    format!("pub fn run(value: i32) -> i32 {{ return value + {delta}; }}")
}

fn invalid_program() -> String {
    "pub fn run(value: i32) -> i32 { return missing; }".into()
}

fn package_inspection(engine: &NexaEngine, package_id: &PackageId) -> PackageInspection {
    engine
        .inspection()
        .packages
        .into_iter()
        .find(|package| package.package_id == *package_id)
        .expect("freshness Package inspection")
}

fn latest_source_missing_identity(
    engine: &NexaEngine,
    package_id: &PackageId,
) -> CandidateIdentity {
    engine
        .pending_development_events
        .iter()
        .rev()
        .find_map(|event| match event {
            DevelopmentEvent::SourceMissing(data) if &data.identity.package_id == package_id => {
                Some(data.identity.clone())
            }
            _ => None,
        })
        .expect("source removal publishes an identity-bearing SourceMissing event")
}

fn assert_run_value(engine: &mut NexaEngine, package_id: &PackageId, expected: i32, context: &str) {
    assert_eq!(
        engine
            .call::<Run>(package_id, &INPUT)
            .unwrap_or_else(|error| panic!("{context}: {error}"))
            .value,
        expected
    );
}

fn exact_host_source_engine(
    identity: nexa::SourceIdentity,
) -> (NexaEngine, PackageId, BuildFingerprint) {
    let package_id =
        PackageId::new("tests.exact_host_registry").expect("exact Host registry Package ID");
    let target = shared_source("exact-host-registry", &package_id, program(1));
    let contract = contract();
    let contract_runtime_id = contract.contract_runtime_id();
    let authority = host_function_authority();
    let mut engine = NexaEngine::builder(contract)
        .host_contract_source(identity, IDL_SOURCE)
        .host_factory(move |_: &PackageContext| {
            Box::new(Registry {
                contract_runtime_id,
                authority: authority.clone(),
            }) as Box<dyn HostRegistry>
        })
        .package_source(target)
        .require_export::<Run>()
        .build()
        .expect("build Engine with exact Host source");
    engine
        .discover()
        .expect("discover Package with exact Host source");
    engine
        .enable_defaults()
        .expect("activate Package with exact Host source");
    let build_fingerprint = package_inspection(&engine, &package_id).build_fingerprint;
    (engine, package_id, build_fingerprint)
}

#[test]
#[allow(clippy::too_many_lines)]
fn host_uri_mutation_invalidates_freshness_and_active_debug_registry() {
    let identity_a = nexa::SourceIdentity::standalone("contracts/active-a.nidl");
    let identity_b = nexa::SourceIdentity::standalone("contracts/active-b.nidl");
    let (mut engine_a, package_a, fingerprint_a) = exact_host_source_engine(identity_a.clone());
    let inspection_a = package_inspection(&engine_a, &package_a);
    let record_a = engine_a
        .packages
        .iter()
        .find(|record| record.candidate.manifest.id == package_a)
        .expect("first exact Host Package record");
    let artifact_a = &record_a
        .runtime
        .as_ref()
        .expect("first exact Host Package is active")
        .artifact;
    let linked_a = artifact_a.linked_state_fingerprint;
    assert_eq!(inspection_a.active_linked_state_fingerprint, Some(linked_a));
    assert_eq!(
        record_a
            .last_known_good
            .as_ref()
            .map(|known_good| known_good.linked_state_fingerprint),
        Some(linked_a)
    );
    assert_eq!(
        artifact_a
            .source_files
            .diagnostic_sources()
            .get(&identity_a)
            .expect("first active registry retains URI A")
            .text(),
        IDL_SOURCE
    );
    assert!(
        artifact_a
            .source_files
            .diagnostic_sources()
            .get(&identity_b)
            .is_none()
    );
    engine_a.shutdown().expect("shutdown URI A Engine");

    let (mut engine_b, package_b, fingerprint_b) = exact_host_source_engine(identity_b.clone());
    assert_ne!(
        fingerprint_a, fingerprint_b,
        "moving an exact Host source must invalidate Engine freshness"
    );
    let inspection_b = package_inspection(&engine_b, &package_b);
    let record_b = engine_b
        .packages
        .iter()
        .find(|record| record.candidate.manifest.id == package_b)
        .expect("second exact Host Package record");
    let artifact_b = &record_b
        .runtime
        .as_ref()
        .expect("second exact Host Package is active")
        .artifact;
    let linked_b = artifact_b.linked_state_fingerprint;
    assert_ne!(
        linked_a, linked_b,
        "moving the Host source must change the active linked-state identity"
    );
    assert_eq!(inspection_b.active_linked_state_fingerprint, Some(linked_b));
    assert_eq!(
        record_b
            .last_known_good
            .as_ref()
            .map(|known_good| known_good.linked_state_fingerprint),
        Some(linked_b)
    );
    assert_eq!(
        artifact_b
            .source_files
            .diagnostic_sources()
            .get(&identity_b)
            .expect("second active registry retains URI B")
            .text(),
        IDL_SOURCE
    );
    assert!(
        artifact_b
            .source_files
            .diagnostic_sources()
            .get(&identity_a)
            .is_none(),
        "the active debug registry must not reuse stale URI A"
    );
    engine_b.shutdown().expect("shutdown URI B Engine");
}

fn active_analysis_revision(engine: &NexaEngine, package_id: &PackageId) -> u64 {
    engine
        .packages
        .iter()
        .find(|record| record.candidate.manifest.id == *package_id)
        .and_then(|record| record.runtime.as_ref())
        .map(|runtime| runtime.artifact.analysis_revision)
        .expect("freshness Package has an active analyzed artifact")
}

fn build_query_stats(engine: &NexaEngine) -> nexa::QueryStats {
    engine
        .build_session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .query_stats()
}

fn terminal_kind(
    engine: &NexaEngine,
    package_id: &PackageId,
    generation: u64,
) -> Option<CandidateTerminalKind> {
    engine
        .packages
        .iter()
        .find(|record| record.candidate.manifest.id == *package_id)
        .and_then(|record| {
            record
                .development
                .terminal_generations
                .get(&generation)
                .copied()
        })
}

fn wait_for_worker_result(engine: &NexaEngine) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline {
        if engine.inspection().development.worker.completed_results > 0 {
            return;
        }
        std::thread::yield_now();
    }
    panic!("the Worker did not publish a completed Result");
}

fn drive_until_terminal(
    fixture: &mut Fixture,
    trace: &mut ScenarioTrace,
    generation: u64,
    expected: CandidateTerminalKind,
    stale_hash: BuildFingerprint,
    stale_value: i32,
) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline {
        if terminal_kind(&fixture.engine, &fixture.target_id, generation) == Some(expected) {
            return;
        }
        trace.tick_checked(
            &mut fixture.engine,
            &fixture.target_id,
            stale_hash,
            stale_value,
        );
        std::thread::yield_now();
    }
    panic!(
        "generation {generation} did not reach terminal {expected:?}: {:?}",
        fixture.engine.inspection().development
    );
}

#[allow(clippy::too_many_arguments)]
fn evidence(
    name: &str,
    stage: &str,
    engine: &NexaEngine,
    package_id: &PackageId,
    trace: &ScenarioTrace,
    stale_generation: u64,
    stale_hash: BuildFingerprint,
    stale_terminal: CandidateTerminalKind,
    expected_created: u64,
    expected_desired_hash: BuildFingerprint,
    expected_hash_mismatch_rejections: u64,
) -> ScenarioEvidence {
    let inspection = engine.inspection();
    let package = inspection
        .packages
        .iter()
        .find(|package| package.package_id == *package_id)
        .expect("freshness target inspection");
    let record = engine
        .packages
        .iter()
        .find(|record| record.candidate.manifest.id == *package_id)
        .expect("freshness target record");
    let stale_candidates_observed = u64::from(trace.events.iter().any(|event| {
        event.data().identity.generation == stale_generation
            && event.data().identity.build_fingerprint == stale_hash
            && matches!(event, DevelopmentEvent::ChangeDetected(_))
    }));
    let stale_candidates_committed = u64::try_from(
        trace
            .reloads
            .iter()
            .filter(|reload| {
                reload.identity.generation == stale_generation
                    && reload.identity.build_fingerprint == stale_hash
                    && reload.outcome == ReloadReportOutcome::Committed
            })
            .count(),
    )
    .expect("stale commit count");
    let superseded_before_compile = u64::try_from(
        record
            .development
            .terminal_generations
            .values()
            .filter(|kind| **kind == CandidateTerminalKind::SupersededBeforeCompile)
            .count(),
    )
    .expect("before-compile terminal count");
    let superseded_after_compile = u64::try_from(
        record
            .development
            .terminal_generations
            .values()
            .filter(|kind| **kind == CandidateTerminalKind::SupersededAfterCompile)
            .count(),
    )
    .expect("after-compile terminal count");
    let desired_build_fingerprint_mismatches_rejected =
        package.desired_build_fingerprint_mismatches_rejected;

    assert_eq!(package.candidate_generation, expected_created);
    assert_eq!(package.terminal_generations, expected_created);
    assert_eq!(package.duplicate_terminals, 0);
    assert_eq!(package.generations_without_terminal, 0);
    assert_eq!(
        package.desired_build_fingerprint,
        Some(expected_desired_hash)
    );
    assert_eq!(
        desired_build_fingerprint_mismatches_rejected,
        expected_hash_mismatch_rejections
    );
    assert_eq!(
        terminal_kind(engine, package_id, stale_generation),
        Some(stale_terminal)
    );
    assert_eq!(stale_candidates_observed, 1);
    assert_eq!(stale_candidates_committed, 0);
    assert_eq!(trace.active_runtime_violations, 0);
    assert_eq!(trace.last_known_good_violations, 0);

    ScenarioEvidence {
        name: name.into(),
        stage: stage.into(),
        stale_candidates_observed,
        stale_candidates_committed,
        desired_build_fingerprint_mismatches_rejected,
        superseded_before_compile,
        superseded_after_compile,
        created_generations: package.candidate_generation,
        terminal_generations: package.terminal_generations,
        duplicate_terminals: package.duplicate_terminals,
        generations_without_terminal: package.generations_without_terminal,
        active_runtime_violations: trace.active_runtime_violations,
        last_known_good_violations: trace.last_known_good_violations,
        status: "PASS".into(),
    }
}

fn run_pending_revert_active() -> ScenarioEvidence {
    let mut fixture = Fixture::new(
        "pendingactive",
        DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        true,
    );
    let mut trace = ScenarioTrace::default();
    let control = fixture.engine.worker_test_control();
    let blocker_id = fixture.blocker_id.clone().expect("blocker ID");
    control.block_before_compile(blocker_id.clone(), 1);
    fixture.change_blocker();
    let stale_hash = fixture.set_target_delta(2);
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    assert!(control.wait_until_blocked(TEST_TIMEOUT));
    let worker = fixture.engine.inspection().development.worker;
    assert_eq!(worker.queued_packages, 1);
    assert_eq!(worker.in_flight_package.as_ref(), Some(&blocker_id));

    fixture.restore_target();
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    control.release();
    assert_eq!(
        terminal_kind(&fixture.engine, &fixture.target_id, 1),
        Some(CandidateTerminalKind::SupersededBeforeCompile)
    );
    assert_eq!(
        fixture
            .engine
            .call::<Run>(&fixture.target_id, &INPUT)
            .expect("A remains active")
            .value,
        ACTIVE_A
    );
    fixture.engine.shutdown().expect("pending Engine shutdown");
    evidence(
        "pending-revert-active",
        "pending",
        &fixture.engine,
        &fixture.target_id,
        &trace,
        1,
        stale_hash,
        CandidateTerminalKind::SupersededBeforeCompile,
        1,
        fixture.initial_hash,
        0,
    )
}

fn run_in_flight_revert_active() -> ScenarioEvidence {
    let mut fixture = Fixture::new(
        "inflightactive",
        DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        false,
    );
    let mut trace = ScenarioTrace::default();
    let control = fixture.engine.worker_test_control();
    control.block_before_compile(fixture.target_id.clone(), 1);
    let stale_hash = fixture.set_target_delta(2);
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    assert!(control.wait_until_blocked(TEST_TIMEOUT));
    assert_eq!(
        fixture
            .engine
            .inspection()
            .development
            .worker
            .in_flight_package
            .as_ref(),
        Some(&fixture.target_id)
    );

    fixture.restore_target();
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    control.release();
    drive_until_terminal(
        &mut fixture,
        &mut trace,
        1,
        CandidateTerminalKind::SupersededAfterCompile,
        stale_hash,
        STALE_B,
    );
    fixture
        .engine
        .shutdown()
        .expect("in-flight Engine shutdown");
    evidence(
        "in-flight-revert-active",
        "in-flight",
        &fixture.engine,
        &fixture.target_id,
        &trace,
        1,
        stale_hash,
        CandidateTerminalKind::SupersededAfterCompile,
        1,
        fixture.initial_hash,
        0,
    )
}

fn run_result_revert_active() -> ScenarioEvidence {
    let mut fixture = Fixture::new(
        "resultactive",
        DevelopmentConfig {
            scan_interval_ticks: 2,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        false,
    );
    let mut trace = ScenarioTrace::default();
    let control = fixture.engine.worker_test_control();
    control.block_before_compile(fixture.target_id.clone(), 1);
    let stale_hash = fixture.set_target_delta(2);
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    assert!(control.wait_until_blocked(TEST_TIMEOUT));
    control.release();
    wait_for_worker_result(&fixture.engine);

    fixture.restore_target();
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    assert_eq!(
        terminal_kind(&fixture.engine, &fixture.target_id, 1),
        Some(CandidateTerminalKind::SupersededAfterCompile)
    );
    fixture.engine.shutdown().expect("result Engine shutdown");
    evidence(
        "result-queue-revert-active",
        "result-queue",
        &fixture.engine,
        &fixture.target_id,
        &trace,
        1,
        stale_hash,
        CandidateTerminalKind::SupersededAfterCompile,
        1,
        fixture.initial_hash,
        1,
    )
}

fn run_manual_ready_revert_active() -> ScenarioEvidence {
    let mut fixture = Fixture::new(
        "manualreadyactive",
        DevelopmentConfig {
            scan_interval_ticks: 3,
            stable_scan_count: 1,
            auto_reload: false,
            ..DevelopmentConfig::default()
        },
        false,
    );
    let mut trace = ScenarioTrace::default();
    let control = fixture.engine.worker_test_control();
    control.block_before_compile(fixture.target_id.clone(), 1);
    let stale_hash = fixture.set_target_delta(2);
    for _ in 0..3 {
        trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    }
    assert!(control.wait_until_blocked(TEST_TIMEOUT));
    control.release();
    wait_for_worker_result(&fixture.engine);
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    assert!(trace.events.iter().any(|event| {
        matches!(event, DevelopmentEvent::CandidateReady(data)
            if data.identity.generation == 1
                && data.identity.build_fingerprint == stale_hash)
    }));

    fixture.restore_target();
    assert!(
        fixture
            .engine
            .request_ready_commit(&fixture.target_id)
            .expect("request Ready commit")
    );
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    assert_eq!(
        terminal_kind(&fixture.engine, &fixture.target_id, 1),
        Some(CandidateTerminalKind::SupersededAfterCompile)
    );
    fixture.engine.shutdown().expect("Ready Engine shutdown");
    evidence(
        "manual-ready-revert-active",
        "ready-candidate",
        &fixture.engine,
        &fixture.target_id,
        &trace,
        1,
        stale_hash,
        CandidateTerminalKind::SupersededAfterCompile,
        1,
        fixture.initial_hash,
        1,
    )
}

fn run_pending_revert_terminal_build_fingerprint() -> ScenarioEvidence {
    let mut fixture = Fixture::new(
        "pendingterminal",
        DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        true,
    );
    let invalid = invalid_program();
    fixture.target.replace(invalid.clone());
    let failed_hash = fixture.target.current_build_fingerprint();
    let mut setup_trace = ScenarioTrace::default();
    drive_until_terminal(
        &mut fixture,
        &mut setup_trace,
        1,
        CandidateTerminalKind::CompileFailed,
        failed_hash,
        STALE_B,
    );
    assert_eq!(
        fixture
            .engine
            .call::<Run>(&fixture.target_id, &INPUT)
            .expect("compile failure keeps A")
            .value,
        ACTIVE_A
    );

    let mut trace = ScenarioTrace::default();
    let control = fixture.engine.worker_test_control();
    let blocker_id = fixture.blocker_id.clone().expect("blocker ID");
    control.block_before_compile(blocker_id.clone(), 1);
    fixture.change_blocker();
    let stale_hash = fixture.set_target_delta(3);
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        stale_hash,
        ACTIVE_C,
    );
    assert!(control.wait_until_blocked(TEST_TIMEOUT));
    let worker = fixture.engine.inspection().development.worker;
    assert_eq!(worker.queued_packages, 1);
    assert_eq!(worker.in_flight_package.as_ref(), Some(&blocker_id));

    fixture.target.replace(invalid);
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        stale_hash,
        ACTIVE_C,
    );
    control.release();
    assert_eq!(
        terminal_kind(&fixture.engine, &fixture.target_id, 2),
        Some(CandidateTerminalKind::SupersededBeforeCompile)
    );
    fixture
        .engine
        .shutdown()
        .expect("terminal-hash Engine shutdown");
    evidence(
        "pending-revert-terminal-hash",
        "pending",
        &fixture.engine,
        &fixture.target_id,
        &trace,
        2,
        stale_hash,
        CandidateTerminalKind::SupersededBeforeCompile,
        2,
        failed_hash,
        0,
    )
}

fn run_result_replaced_by_desired_c() -> ScenarioEvidence {
    let mut fixture = Fixture::new(
        "resultdesired",
        DevelopmentConfig {
            scan_interval_ticks: 2,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        false,
    );
    let mut trace = ScenarioTrace::default();
    let control = fixture.engine.worker_test_control();
    control.block_before_compile(fixture.target_id.clone(), 1);
    let stale_hash = fixture.set_target_delta(2);
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    assert!(control.wait_until_blocked(TEST_TIMEOUT));
    control.release();
    wait_for_worker_result(&fixture.engine);

    let desired_hash = fixture.set_target_delta(3);
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    assert_eq!(
        terminal_kind(&fixture.engine, &fixture.target_id, 1),
        Some(CandidateTerminalKind::SupersededAfterCompile)
    );

    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline {
        let package = package_inspection(&fixture.engine, &fixture.target_id);
        if package.candidate_generation == 2
            && package.terminal_generations == 2
            && package.build_fingerprint == desired_hash
        {
            break;
        }
        trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
        std::thread::yield_now();
    }
    let package = package_inspection(&fixture.engine, &fixture.target_id);
    assert_eq!(package.candidate_generation, 2);
    assert_eq!(package.terminal_generations, 2);
    assert_eq!(package.build_fingerprint, desired_hash);
    assert_eq!(
        fixture
            .engine
            .call::<Run>(&fixture.target_id, &INPUT)
            .expect("C becomes active")
            .value,
        ACTIVE_C
    );
    fixture
        .engine
        .shutdown()
        .expect("desired-C Engine shutdown");
    evidence(
        "result-queue-replaced-by-desired-c",
        "result-queue",
        &fixture.engine,
        &fixture.target_id,
        &trace,
        1,
        stale_hash,
        CandidateTerminalKind::SupersededAfterCompile,
        2,
        desired_hash,
        1,
    )
}

#[test]
fn result_refresh_failure_cancels_and_recovers_without_stale_observation() {
    let mut fixture = Fixture::new(
        "refreshfailure",
        DevelopmentConfig {
            scan_interval_ticks: 2,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        false,
    );
    let mut trace = ScenarioTrace::default();
    let control = fixture.engine.worker_test_control();
    control.block_before_compile(fixture.target_id.clone(), 1);
    let candidate_hash = fixture.set_target_delta(2);
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        candidate_hash,
        STALE_B,
    );
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        candidate_hash,
        STALE_B,
    );
    assert!(control.wait_until_blocked(TEST_TIMEOUT));
    control.release();
    wait_for_worker_result(&fixture.engine);

    fixture.target.set_available(false);
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        candidate_hash,
        STALE_B,
    );
    assert_eq!(
        terminal_kind(&fixture.engine, &fixture.target_id, 1),
        Some(CandidateTerminalKind::CancelledBySourceRemoval)
    );
    let unavailable = package_inspection(&fixture.engine, &fixture.target_id);
    assert_eq!(unavailable.desired_build_fingerprint, None);
    assert_eq!(unavailable.status, PackageStatus::Enabled);

    fixture.target.set_available(true);
    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline {
        let package = package_inspection(&fixture.engine, &fixture.target_id);
        if package.candidate_generation == 2
            && package.terminal_generations == 2
            && package.build_fingerprint == candidate_hash
        {
            break;
        }
        fixture
            .engine
            .tick()
            .expect("recover freshness source after refresh failure");
        std::thread::yield_now();
    }
    let recovered = package_inspection(&fixture.engine, &fixture.target_id);
    assert_eq!(recovered.candidate_generation, 2);
    assert_eq!(recovered.terminal_generations, 2);
    assert_eq!(recovered.duplicate_terminals, 0);
    assert_eq!(recovered.generations_without_terminal, 0);
    assert_eq!(recovered.build_fingerprint, candidate_hash);
    assert_eq!(
        fixture
            .engine
            .call::<Run>(&fixture.target_id, &INPUT)
            .expect("recovered Candidate becomes active")
            .value,
        STALE_B
    );
    fixture
        .engine
        .shutdown()
        .expect("refresh-failure Engine shutdown");
}

#[test]
#[allow(clippy::too_many_lines)]
fn same_hash_aba_keeps_new_generation_worker_identity() {
    let mut fixture = Fixture::new(
        "samehashaba",
        DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        false,
    );
    let mut trace = ScenarioTrace::default();
    let control = fixture.engine.worker_test_control();
    control.block_before_compile_sequence([
        (fixture.target_id.clone(), 1),
        (fixture.target_id.clone(), 3),
    ]);

    let repeated_hash = fixture.set_target_delta(2);
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        repeated_hash,
        STALE_B,
    );
    assert!(control.wait_until_blocked_for(&fixture.target_id, 1, TEST_TIMEOUT));

    fixture.set_target_delta(3);
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        repeated_hash,
        STALE_B,
    );
    fixture.set_target_delta(2);
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        repeated_hash,
        STALE_B,
    );
    assert_eq!(
        terminal_kind(&fixture.engine, &fixture.target_id, 2),
        Some(CandidateTerminalKind::SupersededBeforeCompile)
    );

    control.release();
    assert!(control.wait_until_blocked_for(&fixture.target_id, 3, TEST_TIMEOUT));
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        repeated_hash,
        STALE_B,
    );
    assert_eq!(
        terminal_kind(&fixture.engine, &fixture.target_id, 1),
        Some(CandidateTerminalKind::SupersededAfterCompile)
    );
    let record = fixture
        .engine
        .packages
        .iter()
        .find(|record| record.candidate.manifest.id == fixture.target_id)
        .expect("same-hash ABA target record");
    assert_eq!(record.development.queued_generation, None);
    assert_eq!(record.development.queued_build_fingerprint, None);
    assert_eq!(record.development.in_flight_generation, Some(3));
    assert_eq!(
        record.development.in_flight_build_fingerprint,
        Some(repeated_hash)
    );

    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        repeated_hash,
        STALE_B,
    );
    assert_eq!(
        package_inspection(&fixture.engine, &fixture.target_id).generations_without_terminal,
        1
    );

    control.release();
    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline {
        if terminal_kind(&fixture.engine, &fixture.target_id, 3)
            == Some(CandidateTerminalKind::Compiled)
        {
            break;
        }
        fixture
            .engine
            .tick()
            .expect("complete same-hash ABA Candidate");
        std::thread::yield_now();
    }
    let package = package_inspection(&fixture.engine, &fixture.target_id);
    assert_eq!(package.candidate_generation, 3);
    assert_eq!(package.terminal_generations, 3);
    assert_eq!(package.duplicate_terminals, 0);
    assert_eq!(package.generations_without_terminal, 0);
    assert_eq!(package.build_fingerprint, repeated_hash);
    assert_eq!(
        fixture
            .engine
            .call::<Run>(&fixture.target_id, &INPUT)
            .expect("latest repeated hash becomes active")
            .value,
        STALE_B
    );
    fixture
        .engine
        .shutdown()
        .expect("same-hash ABA Engine shutdown");
}

#[test]
fn manual_reload_discards_diagnostics_from_a_superseded_failed_snapshot() {
    let package_id =
        PackageId::new("tests.manualstaleerror").expect("manual stale error Package ID");
    let shared = shared_source("manual-stale-error", &package_id, program(1));
    let source = SwitchAfterDiscoverySource {
        inner: shared.clone(),
        replacement: Arc::new(RwLock::new(None)),
    };
    let contract = contract();
    let contract_runtime_id = contract.contract_runtime_id();
    let authority = host_function_authority();
    let mut engine = NexaEngine::builder(contract)
        .host_factory(move |_: &PackageContext| {
            Box::new(Registry {
                contract_runtime_id,
                authority: authority.clone(),
            }) as Box<dyn HostRegistry>
        })
        .package_source(source.clone())
        .require_export::<Run>()
        .development(DevelopmentConfig {
            enabled: false,
            ..DevelopmentConfig::default()
        })
        .build()
        .expect("build manual stale-error Engine");
    engine
        .discover()
        .expect("discover manual stale-error Package");
    engine
        .enable_defaults()
        .expect("enable manual stale-error Package");

    shared.replace(invalid_program());
    source.arm(program(3));
    assert!(matches!(
        engine.reload(&package_id),
        Err(EngineError::StaleCandidate(_))
    ));
    let rejected = package_inspection(&engine, &package_id);
    assert_eq!(
        rejected.latest_terminal_kind,
        Some(CandidateTerminalKind::SupersededAfterCompile)
    );
    assert_eq!(
        rejected.recent_diagnostic, None,
        "a failed snapshot rejected by commit-time freshness must not publish diagnostics"
    );
    assert!(
        engine.inspection().recent_diagnostics.is_empty(),
        "the stale failed snapshot must not enter the Engine diagnostic log"
    );
    assert_eq!(
        engine
            .call::<Run>(&package_id, &INPUT)
            .expect("stale failure preserves Last Known Good")
            .value,
        ACTIVE_A
    );

    engine
        .reload(&package_id)
        .expect("the current replacement snapshot remains reloadable");
    assert_eq!(
        engine
            .call::<Run>(&package_id, &INPUT)
            .expect("replacement snapshot becomes active")
            .value,
        ACTIVE_C
    );
    let recovered = package_inspection(&engine, &package_id);
    assert_eq!(recovered.generations_without_terminal, 0);
    assert_eq!(recovered.duplicate_terminals, 0);
    engine
        .shutdown()
        .expect("manual stale-error Engine shutdown");
}

#[test]
fn rediscovery_is_rejected_without_disturbing_an_active_worker_generation() {
    let mut fixture = Fixture::new(
        "rediscovery",
        DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        false,
    );
    let control = fixture.engine.worker_test_control();
    control.block_before_compile(fixture.target_id.clone(), 1);
    let changed = fixture.set_target_delta(2);
    fixture
        .engine
        .tick()
        .expect("queue the changed Candidate before rediscovery");
    assert!(control.wait_until_blocked(TEST_TIMEOUT));

    assert!(matches!(
        fixture.engine.discover(),
        Err(EngineError::DiscoveryAlreadyCompleted)
    ));
    assert_eq!(
        fixture
            .engine
            .call::<Run>(&fixture.target_id, &INPUT)
            .expect("rediscovery rejection preserves the active Realm")
            .value,
        ACTIVE_A
    );

    control.release();
    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline
        && package_inspection(&fixture.engine, &fixture.target_id).build_fingerprint != changed
    {
        fixture
            .engine
            .tick()
            .expect("finish the worker Candidate after rediscovery rejection");
        std::thread::yield_now();
    }
    let package = package_inspection(&fixture.engine, &fixture.target_id);
    assert_eq!(package.build_fingerprint, changed);
    assert_eq!(package.generations_without_terminal, 0);
    assert_eq!(package.duplicate_terminals, 0);
    fixture
        .engine
        .shutdown()
        .expect("rediscovery rejection Engine shutdown");
}

#[test]
fn reload_changed_reports_removal_and_same_input_restore_without_recompile() {
    let mut fixture = Fixture::new(
        "reloadchangedmissing",
        DevelopmentConfig {
            enabled: false,
            ..DevelopmentConfig::default()
        },
        false,
    );
    let before = package_inspection(&fixture.engine, &fixture.target_id);

    fixture.target.set_available(false);
    assert!(matches!(
        fixture.engine.reload_changed(),
        Err(EngineError::Source { .. })
    ));
    let missing = package_inspection(&fixture.engine, &fixture.target_id);
    assert_eq!(missing.status, PackageStatus::Enabled);
    assert_eq!(missing.desired_build_fingerprint, None);
    assert_eq!(
        missing.candidate_generation,
        before.candidate_generation.saturating_add(1)
    );
    assert_eq!(missing.generations_without_terminal, 0);
    let missing_identity = latest_source_missing_identity(&fixture.engine, &fixture.target_id);
    assert_eq!(missing_identity.generation, missing.candidate_generation);
    assert_ne!(
        missing_identity.build_fingerprint, before.build_fingerprint,
        "source absence must not reuse the retained artifact fingerprint"
    );
    assert_eq!(
        fixture
            .engine
            .call::<Run>(&fixture.target_id, &INPUT)
            .expect("source removal retains Last Known Good")
            .value,
        ACTIVE_A
    );

    fixture.target.set_available(true);
    assert_eq!(
        fixture
            .engine
            .reload_changed()
            .expect("same immutable input restores source presence"),
        0
    );
    let restored = package_inspection(&fixture.engine, &fixture.target_id);
    assert_eq!(restored.build_fingerprint, before.build_fingerprint);
    assert_eq!(
        restored.candidate_generation, missing.candidate_generation,
        "restoring unchanged source presence must not create a redundant Candidate"
    );
    assert_eq!(restored.generations_without_terminal, 0);

    fixture.target.set_available(false);
    assert!(matches!(
        fixture.engine.reload_changed(),
        Err(EngineError::Source { .. })
    ));
    let missing_again = package_inspection(&fixture.engine, &fixture.target_id);
    assert_eq!(
        missing_again.candidate_generation,
        missing.candidate_generation.saturating_add(1),
        "each source-missing event must own a new monotonic Generation"
    );
    assert_eq!(missing_again.terminal_generations, 2);
    assert_eq!(missing_again.generations_without_terminal, 0);
    assert_eq!(missing_again.duplicate_terminals, 0);
    let missing_again_identity =
        latest_source_missing_identity(&fixture.engine, &fixture.target_id);
    assert_ne!(
        missing_again_identity.build_fingerprint, missing_identity.build_fingerprint,
        "separate source-missing transitions require separate immutable identities"
    );
    fixture
        .engine
        .shutdown()
        .expect("reload-changed removal Engine shutdown");
}

#[test]
#[allow(clippy::too_many_lines)]
fn reload_changed_reconciles_application_add_delete_and_rename() {
    let app_a = PackageId::new("tests.dynamic.a").expect("dynamic A Package ID");
    let app_b = PackageId::new("tests.dynamic.b").expect("dynamic B Package ID");
    let app_c = PackageId::new("tests.dynamic.c").expect("dynamic C Package ID");
    let source = DynamicApplicationsSource {
        id: SourceId::new("dynamic-applications").expect("dynamic Source ID"),
        policy: PackagePolicy {
            max_packages: 8,
            ..policy()
        },
        applications: Arc::new(RwLock::new(BTreeMap::from([(app_a.clone(), 1)]))),
    };
    let contract = contract();
    let contract_runtime_id = contract.contract_runtime_id();
    let authority = host_function_authority();
    let mut engine = NexaEngine::builder(contract)
        .host_factory(move |_: &PackageContext| {
            Box::new(Registry {
                contract_runtime_id,
                authority: authority.clone(),
            }) as Box<dyn HostRegistry>
        })
        .package_source(source.clone())
        .require_export::<Run>()
        .development(DevelopmentConfig {
            enabled: false,
            ..DevelopmentConfig::default()
        })
        .build()
        .expect("build dynamic application Engine");
    engine
        .discover()
        .expect("discover initial dynamic application");
    engine
        .enable_defaults()
        .expect("enable initial dynamic application");

    source.insert(app_b.clone(), 2);
    assert_eq!(
        engine
            .reload_changed()
            .expect("reconcile added application"),
        1
    );
    assert_eq!(engine.status(&app_b), Some(PackageStatus::Enabled));
    assert_run_value(&mut engine, &app_b, STALE_B, "new application is active");

    source.remove(&app_b);
    source.insert(app_c.clone(), 3);
    assert_eq!(
        engine
            .reload_changed()
            .expect("reconcile renamed application"),
        2
    );
    assert_eq!(engine.status(&app_b), Some(PackageStatus::Enabled));
    assert_run_value(
        &mut engine,
        &app_b,
        STALE_B,
        "renamed-away application retains Last Known Good",
    );
    assert_eq!(engine.status(&app_c), Some(PackageStatus::Enabled));
    assert_run_value(
        &mut engine,
        &app_c,
        ACTIVE_C,
        "renamed application is active",
    );

    let before_delete = package_inspection(&engine, &app_c);
    source.remove(&app_c);
    assert_eq!(
        engine
            .reload_changed()
            .expect("reconcile deleted application"),
        1
    );
    let deleted = package_inspection(&engine, &app_c);
    assert_eq!(deleted.status, PackageStatus::Enabled);
    assert_eq!(deleted.desired_build_fingerprint, None);
    assert_eq!(
        deleted.candidate_generation,
        before_delete.candidate_generation.saturating_add(1),
        "source removal owns a unique terminalized Generation"
    );
    assert_eq!(deleted.generations_without_terminal, 0);
    assert_eq!(deleted.duplicate_terminals, 0);

    source.insert(app_c.clone(), 3);
    assert_eq!(
        engine
            .reload_changed()
            .expect("restore unchanged deleted application"),
        0
    );
    let restored = package_inspection(&engine, &app_c);
    assert_eq!(
        restored.candidate_generation, deleted.candidate_generation,
        "unchanged restoration must not create another Candidate"
    );
    assert_eq!(restored.generations_without_terminal, 0);
    assert_eq!(restored.duplicate_terminals, 0);
    engine
        .shutdown()
        .expect("dynamic application Engine shutdown");
}

#[test]
fn one_build_session_spans_initial_manual_and_worker_builds_with_real_timings() {
    let mut fixture = Fixture::new(
        "sharedsession",
        DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        false,
    );
    let initial_revision = active_analysis_revision(&fixture.engine, &fixture.target_id);
    let initial_stats = build_query_stats(&fixture.engine);

    fixture
        .engine
        .reload(&fixture.target_id)
        .expect("unchanged manual reload through the persistent build session");
    assert_eq!(
        active_analysis_revision(&fixture.engine, &fixture.target_id),
        initial_revision,
        "an unchanged reload must reuse queries without invalidating the session"
    );
    let hot_stats = build_query_stats(&fixture.engine);
    assert!(
        hot_stats.hits > initial_stats.hits,
        "unchanged manual reload must record real query-cache hits"
    );

    fixture.set_target_delta(2);
    fixture
        .engine
        .reload(&fixture.target_id)
        .expect("manual reload through the persistent build session");
    let manual_revision = active_analysis_revision(&fixture.engine, &fixture.target_id);
    assert!(
        manual_revision > initial_revision,
        "manual reload must advance the initial build session revision"
    );

    let worker_fingerprint = fixture.set_target_delta(3);
    let deadline = Instant::now() + TEST_TIMEOUT;
    let worker_report = loop {
        assert!(
            Instant::now() < deadline,
            "development worker did not commit the changed package"
        );
        let report = fixture
            .engine
            .tick()
            .expect("tick shared-session development worker");
        if let Some(reload) = report.reloads.into_iter().find(|reload| {
            reload.identity.build_fingerprint == worker_fingerprint
                && reload.outcome == ReloadReportOutcome::Committed
        }) {
            break reload;
        }
        std::thread::yield_now();
    };

    let worker_revision = active_analysis_revision(&fixture.engine, &fixture.target_id);
    assert!(
        worker_revision > manual_revision,
        "worker build must advance the same session used by manual reload"
    );
    assert!(
        worker_report.compile_duration > Duration::ZERO,
        "compile timing must measure canonical analysis/codegen work"
    );
    assert!(
        worker_report.verify_duration > Duration::ZERO,
        "verify timing must measure the façade verifier invocation"
    );
    assert_eq!(
        fixture
            .engine
            .call::<Run>(&fixture.target_id, &INPUT)
            .expect("worker artifact is active")
            .value,
        ACTIVE_C
    );
    fixture
        .engine
        .shutdown()
        .expect("shared-session Engine shutdown");
}

#[test]
fn candidate_freshness_machine_report_uses_real_engine_evidence() {
    let scenarios = vec![
        run_pending_revert_active(),
        run_in_flight_revert_active(),
        run_result_revert_active(),
        run_manual_ready_revert_active(),
        run_pending_revert_terminal_build_fingerprint(),
        run_result_replaced_by_desired_c(),
    ];
    let sum = |select: fn(&ScenarioEvidence) -> u64| scenarios.iter().map(select).sum::<u64>();
    let stage_count = |stage: &str| {
        u64::try_from(
            scenarios
                .iter()
                .filter(|scenario| scenario.stage == stage)
                .count(),
        )
        .expect("stage count")
    };
    let report = FreshnessReport {
        schema: 1,
        scenario_count: u64::try_from(scenarios.len()).expect("scenario count"),
        pending_scenario_count: stage_count("pending"),
        in_flight_scenario_count: stage_count("in-flight"),
        result_queue_scenario_count: stage_count("result-queue"),
        ready_candidate_scenario_count: stage_count("ready-candidate"),
        stale_candidates_observed: sum(|scenario| scenario.stale_candidates_observed),
        stale_candidates_committed: sum(|scenario| scenario.stale_candidates_committed),
        desired_build_fingerprint_mismatches_rejected: sum(|scenario| {
            scenario.desired_build_fingerprint_mismatches_rejected
        }),
        superseded_before_compile: sum(|scenario| scenario.superseded_before_compile),
        superseded_after_compile: sum(|scenario| scenario.superseded_after_compile),
        created_generations: sum(|scenario| scenario.created_generations),
        terminal_generations: sum(|scenario| scenario.terminal_generations),
        duplicate_terminals: sum(|scenario| scenario.duplicate_terminals),
        generations_without_terminal: sum(|scenario| scenario.generations_without_terminal),
        active_runtime_violations: sum(|scenario| scenario.active_runtime_violations),
        last_known_good_violations: sum(|scenario| scenario.last_known_good_violations),
        scenarios,
        status: "PASS".into(),
    };

    assert_eq!(report.scenario_count, 6);
    assert_eq!(report.pending_scenario_count, 2);
    assert_eq!(report.in_flight_scenario_count, 1);
    assert_eq!(report.result_queue_scenario_count, 2);
    assert_eq!(report.ready_candidate_scenario_count, 1);
    assert_eq!(report.stale_candidates_observed, 6);
    assert_eq!(report.stale_candidates_committed, 0);
    assert_eq!(report.desired_build_fingerprint_mismatches_rejected, 3);
    assert_eq!(report.superseded_before_compile, 2);
    assert_eq!(report.superseded_after_compile, 4);
    assert_eq!(report.created_generations, 8);
    assert_eq!(report.terminal_generations, 8);
    assert_eq!(report.duplicate_terminals, 0);
    assert_eq!(report.generations_without_terminal, 0);
    assert_eq!(report.active_runtime_violations, 0);
    assert_eq!(report.last_known_good_violations, 0);

    let mut rendered = serde_json::to_string_pretty(&report).expect("serialize freshness report");
    rendered.push('\n');
    if let Some(path) = std::env::var_os("NEXA_CANDIDATE_FRESHNESS_REPORT") {
        let path = PathBuf::from(path);
        std::fs::create_dir_all(path.parent().expect("freshness report parent"))
            .expect("create freshness report directory");
        std::fs::write(path, &rendered).expect("write Candidate freshness report");
    }
    println!("{rendered}");
}
