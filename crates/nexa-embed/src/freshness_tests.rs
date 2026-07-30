use super::*;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use nexa_runtime::{
    HostCallOutcome, HostRegistry, HostTrap, ResourceContext, RuntimeHostArgs, RuntimeValue,
    ScriptArgumentRequirements, ScriptCallError, ScriptCallWriter, ScriptExport,
    ScriptOutputReader, Signature, StableId, ValueType,
};
use serde::Serialize;

const IDL_SOURCE: &str = "interface TestHost {
    enum WaitError { Cancelled }
    request(return_error, trap) fn wait(value: i32) -> request<Result<i32, WaitError>>;
    export Run(value: i32) -> i32;
}";
const RUN_ID: StableId = StableId(0xf1c5_6273_0ddd_ab52);
const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const INPUT: i32 = 10;
const ACTIVE_A: i32 = 11;
const STALE_B: i32 = 12;
const ACTIVE_C: i32 = 13;

struct Registry(StableId);

impl HostRegistry for Registry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call_runtime(
        &mut self,
        _: u32,
        _: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::UnknownFunction(0))
    }
}

struct Run;

impl ScriptExport for Run {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = RUN_ID;
    const NAME: &'static str = "Run";

    fn signature() -> Signature {
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        }
    }

    fn argument_requirements(
        _: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Ok(ScriptArgumentRequirements::ZERO)
    }

    fn encode_args(
        _: &mut ScriptCallWriter<'_>,
        args: &Self::Args,
    ) -> Result<Vec<RuntimeValue>, ScriptCallError> {
        Ok(vec![RuntimeValue::I32(*args)])
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

    fn current_hash(&self) -> SourceHash {
        let candidate = self
            .discover()
            .expect("freshness source discovery")
            .remove(0);
        candidate_source_hash(&candidate)
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

    fn discover(&self) -> Result<Vec<PackageCandidate>, PackageSourceError> {
        if !*self.available.read().expect("freshness availability lock") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "freshness source is unavailable",
            )
            .into());
        }
        let manifest = PackageManifest::parse(&self.manifest, &self.policy)?;
        Ok(vec![PackageCandidate::new(
            manifest,
            self.manifest.clone(),
            self.script.read().expect("freshness source lock").clone(),
        )])
    }
}

struct Fixture {
    engine: NexaEngine,
    target: SharedSource,
    target_id: PackageId,
    initial_source: String,
    initial_hash: SourceHash,
    blocker: Option<SharedSource>,
    blocker_id: Option<PackageId>,
}

impl Fixture {
    fn new(label: &str, config: DevelopmentConfig, with_blocker: bool) -> Self {
        let target_id = PackageId::new(format!("tests.freshness{label}")).expect("target ID");
        let initial_source = program(target_id.as_str(), 1);
        let target = shared_source(
            &format!("freshness-{label}"),
            &target_id,
            initial_source.clone(),
        );
        let initial_hash = target.current_hash();
        let contract = contract();
        let interface_hash = contract.interface_hash;
        let mut builder = NexaEngine::builder(contract)
            .host_factory(move |_: &PackageContext| {
                Box::new(Registry(interface_hash)) as Box<dyn HostRegistry>
            })
            .require_export::<Run>()
            .development(config);

        let (blocker, blocker_id) = if with_blocker {
            let blocker_id =
                PackageId::new(format!("tests.freshness{label}blocker")).expect("blocker ID");
            let blocker = shared_source(
                &format!("freshness-{label}-blocker"),
                &blocker_id,
                program(blocker_id.as_str(), 1),
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

    fn set_target_delta(&self, delta: i32) -> SourceHash {
        self.target.replace(program(self.target_id.as_str(), delta));
        self.target.current_hash()
    }

    fn restore_target(&self) {
        self.target.replace(self.initial_source.clone());
    }

    fn change_blocker(&self) {
        let blocker = self.blocker.as_ref().expect("fixture has a blocker");
        let blocker_id = self.blocker_id.as_ref().expect("fixture has a blocker ID");
        blocker.replace(program(blocker_id.as_str(), 9));
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
        stale_hash: SourceHash,
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
        stale_hash: SourceHash,
        stale_value: i32,
    ) {
        let value = engine
            .call::<Run>(package_id, &INPUT)
            .expect("freshness Runtime remains callable")
            .value;
        if value == stale_value {
            self.active_runtime_violations = self.active_runtime_violations.saturating_add(1);
        }
        let active_hash = package_inspection(engine, package_id).source_hash;
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
    desired_hash_mismatches_rejected: u64,
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
    desired_hash_mismatches_rejected: u64,
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
    let idl = nexa_idl::parse(IDL_SOURCE).expect("freshness test IDL");
    HostContract {
        interface_name: "TestHost",
        canonical_idl: IDL_SOURCE,
        interface_hash: nexa_idl::exact_hash(&idl),
        generator_schema_version: nexa_runtime::HOST_CONTRACT_SCHEMA_VERSION,
    }
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
            "schema = 1\n\
             id = \"{package_id}\"\n\
             name = \"Freshness\"\n\
             version = \"1.0.0\"\n\
             entry = \"main.nexa\"\n\
             activation = \"default-enabled\"\n\
             handler_fuel = 20000\n\
             capabilities = []\n"
        ),
        script: Arc::new(RwLock::new(script)),
        available: Arc::new(RwLock::new(true)),
    }
}

fn program(module: &str, delta: i32) -> String {
    format!(
        "module {module};\nimport test;\n\
         fn Run(value: i32) -> i32 {{ return value + {delta}; }}"
    )
}

fn invalid_program(module: &str) -> String {
    format!(
        "module {module};\nimport test;\n\
         fn Run(value: i32) -> i32 {{ return missing; }}"
    )
}

fn package_inspection(engine: &NexaEngine, package_id: &PackageId) -> PackageInspection {
    engine
        .inspection()
        .packages
        .into_iter()
        .find(|package| package.package_id == *package_id)
        .expect("freshness Package inspection")
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
    stale_hash: SourceHash,
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
    stale_hash: SourceHash,
    stale_terminal: CandidateTerminalKind,
    expected_created: u64,
    expected_desired_hash: SourceHash,
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
        event.data().candidate_generation == stale_generation
            && event.data().source_hash == stale_hash
            && matches!(event, DevelopmentEvent::ChangeDetected(_))
    }));
    let stale_candidates_committed = u64::try_from(
        trace
            .reloads
            .iter()
            .filter(|reload| {
                reload.candidate_generation == stale_generation
                    && reload.source_hash == stale_hash
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
    let desired_hash_mismatches_rejected = package.desired_hash_mismatches_rejected;

    assert_eq!(package.candidate_generation, expected_created);
    assert_eq!(package.terminal_generations, expected_created);
    assert_eq!(package.duplicate_terminals, 0);
    assert_eq!(package.generations_without_terminal, 0);
    assert_eq!(package.desired_hash, Some(expected_desired_hash));
    assert_eq!(
        desired_hash_mismatches_rejected,
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
        desired_hash_mismatches_rejected,
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
    control.block_before_compile(blocker_id, 1);
    fixture.change_blocker();
    let stale_hash = fixture.set_target_delta(2);
    trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
    assert!(control.wait_until_blocked(TEST_TIMEOUT));
    assert_eq!(fixture.engine.inspection().development.queued_candidates, 1);

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
            if data.candidate_generation == 1 && data.source_hash == stale_hash)
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

fn run_pending_revert_terminal_hash() -> ScenarioEvidence {
    let mut fixture = Fixture::new(
        "pendingterminal",
        DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        },
        true,
    );
    let invalid = invalid_program(fixture.target_id.as_str());
    fixture.target.replace(invalid.clone());
    let failed_hash = fixture.target.current_hash();
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
    control.block_before_compile(blocker_id, 1);
    fixture.change_blocker();
    let stale_hash = fixture.set_target_delta(3);
    trace.tick_checked(
        &mut fixture.engine,
        &fixture.target_id,
        stale_hash,
        ACTIVE_C,
    );
    assert!(control.wait_until_blocked(TEST_TIMEOUT));
    assert_eq!(fixture.engine.inspection().development.queued_candidates, 1);

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
            && package.source_hash == desired_hash
        {
            break;
        }
        trace.tick_checked(&mut fixture.engine, &fixture.target_id, stale_hash, STALE_B);
        std::thread::yield_now();
    }
    let package = package_inspection(&fixture.engine, &fixture.target_id);
    assert_eq!(package.candidate_generation, 2);
    assert_eq!(package.terminal_generations, 2);
    assert_eq!(package.source_hash, desired_hash);
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
    assert_eq!(unavailable.desired_hash, None);
    assert_eq!(unavailable.status, PackageStatus::Enabled);

    fixture.target.set_available(true);
    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline {
        let package = package_inspection(&fixture.engine, &fixture.target_id);
        if package.candidate_generation == 2
            && package.terminal_generations == 2
            && package.source_hash == candidate_hash
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
    assert_eq!(recovered.source_hash, candidate_hash);
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
    assert_eq!(record.development.queued_hash, None);
    assert_eq!(record.development.in_flight_generation, Some(3));
    assert_eq!(record.development.in_flight_hash, Some(repeated_hash));

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
    assert_eq!(package.source_hash, repeated_hash);
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
fn candidate_freshness_machine_report_uses_real_engine_evidence() {
    let scenarios = vec![
        run_pending_revert_active(),
        run_in_flight_revert_active(),
        run_result_revert_active(),
        run_manual_ready_revert_active(),
        run_pending_revert_terminal_hash(),
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
        desired_hash_mismatches_rejected: sum(|scenario| scenario.desired_hash_mismatches_rejected),
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
    assert_eq!(report.desired_hash_mismatches_rejected, 3);
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
