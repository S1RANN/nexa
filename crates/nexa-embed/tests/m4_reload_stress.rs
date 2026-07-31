use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use nexa::prelude::{
    HostCallOutcome, HostRegistry, HostTrap, ResourceContext, RuntimeValue, Signature, StableId,
    ValueType,
};
use nexa::{
    RuntimeHostArgs, ScriptArgumentRequirements, ScriptCallError, ScriptCallWriter, ScriptExport,
    ScriptOutputReader,
};
use nexa_embed::{
    ActivationPolicy, ActivationSet, CandidateBuildContext, CandidateIdentity,
    CandidateTerminalKind, CapabilitySet, DevelopmentConfig, DevelopmentEvent, DiscoveredPackage,
    EngineDiagnosticStage, EngineHealth, EngineTickReport, HostContract, MemoryPackage,
    MemorySource, NexaEngine, PackageContext, PackageId, PackagePolicy, PackageRuntimeLimits,
    PackageSource, PackageSourceError, ReloadReport, ReloadReportOutcome, SourceId, TrustLevel,
};
use serde::Serialize;

const IDL: &str = "interface TestHost {
    enum WaitError { Cancelled }
    request(return_error, trap) fn wait(value: i32) -> request<Result<i32, WaitError>>;
    export Run(value: i32) -> i32;
}";
const RUN_ID: StableId = StableId(0xf1c5_6273_0ddd_ab52);
const ITERATIONS_PER_CLASS: u64 = 100;
const ACTIVATION_RECOVERIES: u64 = 10;
const WORKER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct ProjectSource {
    id: SourceId,
    policy: PackagePolicy,
    state: Arc<RwLock<ProjectState>>,
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
                "module stress.app;\nimport support.library as support;\nimport host as stress;\n\
                 @stateful({}) class Store {{ value: i32; }}\n\
                 pub fn Run(value: i32) -> i32 {{ return support.revision() + value + {}; }}\n",
                self.state_version, self.delta,
            )
        })
    }

    fn dependency_source(&self) -> String {
        format!(
            "module library;\npub fn revision() -> i32 {{ return {}; }}\n",
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
    interface_hash: StableId,
    requests_created: Arc<AtomicU64>,
}

impl HostRegistry for Registry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.interface_hash)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 {
            return Err(HostTrap::UnknownFunction(id));
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
        value: &Self::Args,
    ) -> Result<Vec<RuntimeValue>, ScriptCallError> {
        Ok(vec![RuntimeValue::I32(*value)])
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

#[test]
#[ignore = "M4 machine-evidence stress gate"]
#[allow(clippy::too_many_lines)]
fn m4_reload_stress() {
    let idl = nexa::parse(IDL).expect("stress Host contract");
    let contract = HostContract {
        interface_name: "TestHost",
        canonical_idl: IDL,
        interface_hash: nexa::exact_hash(&idl),
        generator_schema_version: nexa::HOST_CONTRACT_SCHEMA_VERSION,
    };
    let interface_hash = contract.interface_hash;
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
                interface_hash,
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
        "module stress.app;\nimport support.library as support;\nimport host as stress;\n\
         @stateful(1) class Store { value: i32; }\n\
         pub task fn Run(value: i32) -> i32 {\n\
             let result: Result<i32, stress.WaitError> = await stress.wait(value);\n\
             return match result { Ok(found) => found, Err(error) => 0 };\n\
         }\n"
        .into(),
    );
    engine
        .reload(&package_id)
        .expect("load the Host request resource probe");
    assert!(
        engine.call::<Run>(&package_id, &7).is_err(),
        "a must-complete call unexpectedly retained an unresolved Host request"
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
            .root_override = Some(format!("module stress.app;\n# invalid_{iteration}\n"));
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
            "module stress.app;\nimport support.library as support;\nimport host as stress;\n\
                 @stateful(1) class Store {{ value: i32; }}\n\
                 pub fn Run(value: i32) -> i32 {{ return support.revision() + missing_{iteration}; }}\n"
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
            "module stress.app;\nimport support.library as support;\nimport host as stress;\n\
                 @stateful(1) class Store {{ value: i32; }}\n\
                 pub immediate fn Run(value: i32) -> i32 {{\n\
                     for step in 0..{bound} {{\n\
                         if step == {} {{ return value; }}\n\
                     }}\n\
                     return support.revision() + value;\n\
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
                "module stress.extra;\npub fn marker() -> i32 { return 1; }\n".into(),
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
        project.extra_sources.insert(
            "src/stress/renamed.nexa".into(),
            renamed_source.replace("stress.extra", "stress.renamed"),
        );
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
        state
            .write()
            .expect("stress project write lock")
            .root_override = Some(format!(
            "module stress.app;\nimport support.library as support;\nimport host as stress;\n\
                 @stateful(1) class Store {{ value: i32; }}\n\
                 pub fn Run(value: i32) -> i32 {{ return support.revision() + value; }}\n\
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
    assert!(report.safety.values().all(|count| *count == 0));

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
        .discover(&CandidateBuildContext::new(IDL.as_bytes().to_vec()))
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
