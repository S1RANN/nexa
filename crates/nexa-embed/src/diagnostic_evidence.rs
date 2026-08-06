use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use nexa::prelude::{
    FunctionEffect, HostCallOutcome, HostCompletionTicket, HostFunctionAuthority, HostFunctionSlot,
    HostRegistry, HostTrap, ResolvedHostFunction, ResourceContext, RuntimeHostArgs, RuntimeValue,
    ScriptArgumentRequirements, ScriptArguments, ScriptCallError, ScriptCallWriter, ScriptExport,
    ScriptOutputReader, ScriptSignature, StableId, ValueType,
};
use serde::Deserialize;

use crate::{
    ActivationPolicy, ActivationSet, CandidateBuildContext, CapabilitySet, DiagnosticRenderer,
    DiscoveredPackage, EngineDiagnostic, EngineError, MemoryPackage, MemorySource, NexaEngine,
    PackageId, PackagePolicy, PackageRuntimeLimits, PackageSource, PackageSourceError, SourceId,
    TrustLevel,
};

const IDL_SOURCE: &str = "contract Test {
    enum WaitError { Cancelled, }
    host {
        @cancel(return_error)
        @abandon(trap)
        async fn wait(value: i32) -> Result<i32, WaitError>;
    }
    nexa {
        fn run(value: i32) -> i32;
        fn missing_run(value: i32) -> i32;
    }
}";
const TASK_IDL_SOURCE: &str = "contract Test {
    enum WaitError { Cancelled, }
    host {
        @cancel(return_error)
        @abandon(trap)
        async fn wait(value: i32) -> Result<i32, WaitError>;
    }
    nexa {
        async fn run(value: i32) -> i32;
        fn missing_run(value: i32) -> i32;
    }
}";
const RUN_ID: StableId = StableId(0x7b5d_1e73_ccb5_ad74);
const MISSING_RUN_ID: StableId = StableId(0xd1ad_b999_51e8_0fcd);
const EVIDENCE_ENTRY: &str = "evidence.valid";
static EVIDENCE_RUN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static EVIDENCE_CONTRACT: OnceLock<crate::HostContract> = OnceLock::new();
static EVIDENCE_TASK_CONTRACT: OnceLock<crate::HostContract> = OnceLock::new();
static EVIDENCE_HOST_AUTHORITY: OnceLock<HostFunctionAuthority> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct EngineDiagnosticEvidence {
    pub code: nexa::ErrorCode,
    pub public_entrypoint: &'static str,
    pub diagnostic: EngineDiagnostic,
    pub human: String,
    pub json: String,
    pub ndjson: String,
    pub rendering: EngineRenderEvidence,
    pub observation: EngineDiagnosticObservation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineRenderEvidence {
    pub human_matches_diagnostic: bool,
    pub json_matches_diagnostic: bool,
    pub ndjson_matches_diagnostic: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineDiagnosticObservation {
    pub real_engine_path: bool,
    pub source_evidence_valid: bool,
    pub deterministic: bool,
}

#[derive(Debug, Deserialize)]
struct EngineFixture {
    version: u32,
    code: String,
    pipeline: String,
    expected: EngineFixtureExpected,
}

#[derive(Debug, Deserialize)]
struct EngineFixtureExpected {
    message_contains: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RenderedDiagnostic {
    schema: u32,
    position_encoding: String,
    sequence: u64,
    code: String,
    severity: String,
    file: Option<String>,
    source_identity: Option<String>,
    range: Option<RenderedRange>,
    message: String,
    related: Vec<RenderedRelated>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
struct RenderedPosition {
    line: u32,
    character: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
struct RenderedRange {
    start: RenderedPosition,
    end: RenderedPosition,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RenderedRelated {
    message: String,
    file: Option<String>,
    source_identity: Option<String>,
    range: Option<RenderedRange>,
}

struct Registry {
    contract_runtime_id: StableId,
    authority: HostFunctionAuthority,
    held_completion: Option<Arc<Mutex<Option<HostCompletionTicket>>>>,
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
        let request = pending.request;
        if let Some(held) = &self.held_completion {
            *held.lock().expect("completion evidence lock") = Some(pending.ticket);
        }
        Ok(HostCallOutcome::Pending(request))
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

struct TaskRun;

impl ScriptExport for TaskRun {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = RUN_ID;
    const NAME: &'static str = "run";
    const CONTRACT_SLOT: usize = 1;
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

struct MissingRun;

impl ScriptExport for MissingRun {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = MISSING_RUN_ID;
    const NAME: &'static str = "missing_run";
    const CONTRACT_SLOT: usize = 0;
    const SIGNATURE: ScriptSignature = Run::SIGNATURE;
    const EFFECT: FunctionEffect = FunctionEffect::Ordinary;

    fn argument_requirements(
        _: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Ok(ScriptArgumentRequirements::ZERO)
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

struct FailingSource {
    id: SourceId,
    policy: PackagePolicy,
}

impl PackageSource for FailingSource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn policy(&self) -> &PackagePolicy {
        &self.policy
    }

    fn discover(
        &self,
        _: &CandidateBuildContext,
    ) -> Result<Vec<DiscoveredPackage>, PackageSourceError> {
        Err(PackageSourceError::Io(std::io::Error::other(
            "diagnostic evidence source unavailable",
        )))
    }
}

#[derive(Clone)]
struct SharedSource {
    id: SourceId,
    policy: PackagePolicy,
    manifest: String,
    script: Arc<RwLock<String>>,
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
        let script = self.script.read().expect("source evidence lock").clone();
        let module = EVIDENCE_ENTRY;
        let manifest = self.manifest.replace(
            "entry = \"evidence.valid\"",
            &format!("entry = \"{module}\""),
        );
        MemorySource::new(self.id.clone(), self.policy.clone())
            .package(
                MemoryPackage::new(self.id.as_str(), manifest)
                    .source(module_source_path(module), script),
            )
            .discover(build)
    }
}

pub fn run_engine_diagnostic_evidence(
    root: &Path,
) -> Result<Vec<EngineDiagnosticEvidence>, String> {
    let _run = EVIDENCE_RUN_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Engine diagnostic evidence lock was poisoned".to_owned())?;
    let registered = engine_codes();
    let mut evidence = Vec::with_capacity(registered.len());
    for code in registered {
        validate_fixture(root, code)?;
        let mut first = execute_scenario(code)?;
        let second = execute_scenario(code)?;
        first.observation.deterministic = deterministically_equivalent(&first, &second);
        evidence.push(first);
    }
    Ok(evidence)
}

pub fn run_engine_diagnostic_cases(root: &Path) -> Result<nexa::EngineDiagnosticReport, String> {
    let registered = engine_codes();
    let evidence = run_engine_diagnostic_evidence(root)?;
    let direct_diagnostic_construction = direct_diagnostic_construction_count(root)?;
    let mut cases = Vec::with_capacity(evidence.len());
    for item in &evidence {
        let fixture = load_fixture(root, item.code)?;
        let primary = observed_primary(item)?;
        let passed = item.observation.real_engine_path
            && item.diagnostic.diagnostic.code == item.code
            && item.rendering.human_matches_diagnostic
            && item.rendering.json_matches_diagnostic
            && item.rendering.ndjson_matches_diagnostic
            && item.observation.source_evidence_valid
            && item.observation.deterministic
            && direct_diagnostic_construction == 0
            && normalized(&item.diagnostic.diagnostic.message.to_string())
                .contains(&normalized(&fixture.expected.message_contains));
        cases.push(nexa::ObservedDiagnosticCase {
            case_id: item.code.to_string(),
            code: item.code.to_string(),
            observed: item.diagnostic.diagnostic.code.to_string(),
            pipeline: "engine".into(),
            category: "diagnostic".into(),
            primary_text: primary.text,
            primary_start: primary.start,
            primary_end: primary.end,
            secondary_count: item.diagnostic.related.len(),
            human_output: item.rendering.human_matches_diagnostic,
            json_output: item.rendering.json_matches_diagnostic
                && item.rendering.ndjson_matches_diagnostic,
            passed,
        });
    }
    Ok(nexa::EngineDiagnosticReport {
        registered: registered.len(),
        observed_through_real_paths: evidence
            .iter()
            .filter(|item| item.observation.real_engine_path)
            .count(),
        direct_diagnostic_construction,
        human_output: evidence
            .iter()
            .filter(|item| item.rendering.human_matches_diagnostic)
            .count(),
        json_output: evidence
            .iter()
            .filter(|item| item.rendering.json_matches_diagnostic)
            .count(),
        ndjson_output: evidence
            .iter()
            .filter(|item| item.rendering.ndjson_matches_diagnostic)
            .count(),
        deterministic: evidence
            .iter()
            .filter(|item| item.observation.deterministic)
            .count(),
        codes: evidence.iter().map(|item| item.code.to_string()).collect(),
        cases,
    })
}

fn engine_codes() -> Vec<nexa::ErrorCode> {
    nexa::ERROR_CODE_TABLE
        .iter()
        .filter(|definition| definition.code.as_str().starts_with("NX7"))
        .map(|definition| definition.code)
        .collect()
}

fn execute_scenario(code: nexa::ErrorCode) -> Result<EngineDiagnosticEvidence, String> {
    match code {
        nexa::ErrorCode::NX7001 => source_failure(),
        nexa::ErrorCode::NX7002 => manifest_failure(),
        nexa::ErrorCode::NX7003 => policy_failure(),
        nexa::ErrorCode::NX7004 => entitlement_failure(),
        nexa::ErrorCode::NX7010 => missing_export(),
        nexa::ErrorCode::NX7011 => signature_mismatch(),
        nexa::ErrorCode::NX7101 => handler_yield(),
        nexa::ErrorCode::NX7102 => handler_wait(),
        nexa::ErrorCode::NX7103 => handler_trap(),
        nexa::ErrorCode::NX7201 => migration_rollback(),
        nexa::ErrorCode::NX7202 => activation_fault(),
        nexa::ErrorCode::NX7302 => persistence_failure(),
        nexa::ErrorCode::NX7303 => shutdown_failure(),
        _ => Err(format!("{code} has no Engine evidence scenario")),
    }
}

fn source_failure() -> Result<EngineDiagnosticEvidence, String> {
    let source = FailingSource {
        id: source_id("evidence-source")?,
        policy: policy([ActivationPolicy::UserControlled]),
    };
    let mut engine = builder(source, None).build().map_err(string_error)?;
    engine
        .discover()
        .expect_err("the evidence source must fail discovery");
    capture(&engine, nexa::ErrorCode::NX7001, "NexaEngine::discover")
}

fn manifest_failure() -> Result<EngineDiagnosticEvidence, String> {
    let source = MemorySource::new(
        source_id("evidence-manifest")?,
        policy([ActivationPolicy::UserControlled]),
    )
    .package(
        MemoryPackage::new(
            "evidence.manifest",
            manifest("evidence.manifest", "user-controlled", "")
                .replace("schema = 2", "schema = 9"),
        )
        .source("src/evidence/valid.nexa", valid_script(1)),
    );
    let mut engine = builder(source, None).build().map_err(string_error)?;
    engine
        .discover()
        .expect_err("the invalid manifest must fail discovery");
    capture(&engine, nexa::ErrorCode::NX7002, "NexaEngine::discover")
}

fn policy_failure() -> Result<EngineDiagnosticEvidence, String> {
    let source = MemorySource::new(
        source_id("evidence-policy")?,
        policy([ActivationPolicy::UserControlled]),
    )
    .package(
        MemoryPackage::new(
            "evidence.policy",
            manifest("evidence.policy", "user-controlled", "")
                .replace("capabilities = []", "capabilities = [\"evidence.denied\"]"),
        )
        .source("src/evidence/valid.nexa", valid_script(1)),
    );
    let mut engine = builder(source, None).build().map_err(string_error)?;
    engine
        .discover()
        .expect_err("the denied capability must fail discovery");
    capture(&engine, nexa::ErrorCode::NX7003, "NexaEngine::discover")
}

fn entitlement_failure() -> Result<EngineDiagnosticEvidence, String> {
    let package_id = package_id("evidence.entitlement")?;
    let source = memory_source(
        "evidence-entitlement",
        package_id.as_str(),
        "user-controlled",
        "evidence.license",
        &valid_script(1),
    )?;
    let mut engine = builder(source, None).build().map_err(string_error)?;
    engine.discover().map_err(string_error)?;
    assert!(matches!(
        engine.enable(&package_id),
        Err(EngineError::Locked(_))
    ));
    capture(&engine, nexa::ErrorCode::NX7004, "NexaEngine::enable")
}

fn missing_export() -> Result<EngineDiagnosticEvidence, String> {
    let package_id = package_id("evidence.missing-export")?;
    let source = memory_source(
        "evidence-missing-export",
        package_id.as_str(),
        "user-controlled",
        "",
        &valid_script(1),
    )?;
    let mut engine = builder(source, None)
        .require_export::<MissingRun>()
        .build()
        .map_err(string_error)?;
    engine.discover().map_err(string_error)?;
    engine
        .enable(&package_id)
        .expect_err("required export must be missing");
    capture(&engine, nexa::ErrorCode::NX7010, "NexaEngine::enable")
}

fn signature_mismatch() -> Result<EngineDiagnosticEvidence, String> {
    let package_id = package_id("evidence.signature")?;
    let source = memory_source(
        "evidence-signature",
        package_id.as_str(),
        "user-controlled",
        "",
        "use host::test;\n\
         pub fn run(value: i64) -> i32 { return 1; }",
    )?;
    let mut engine = builder(source, None)
        .require_export::<Run>()
        .build()
        .map_err(string_error)?;
    engine.discover().map_err(string_error)?;
    engine
        .enable(&package_id)
        .expect_err("required export signature must be rejected");
    capture(&engine, nexa::ErrorCode::NX7011, "NexaEngine::enable")
}

fn handler_yield() -> Result<EngineDiagnosticEvidence, String> {
    task_handler_failure(
        "evidence-yield",
        "evidence.handler-yield",
        "pub async fn run(value: i32) -> i32 { yield; return value; }",
        nexa::ErrorCode::NX7101,
    )
}

fn handler_wait() -> Result<EngineDiagnosticEvidence, String> {
    task_handler_failure(
        "evidence-wait",
        "evidence.handler-wait",
        "pub async fn run(value: i32) -> i32 {
            let result: Result<i32, test::WaitError> = test::wait(value).await;
            return match result {
                Result::Ok(found) => found,
                Result::Err(error) => 0,
            };
        }",
        nexa::ErrorCode::NX7102,
    )
}

fn handler_trap() -> Result<EngineDiagnosticEvidence, String> {
    ordinary_handler_failure(
        "evidence-trap",
        "evidence.handler-trap",
        "pub fn run(value: i32) -> i32 {
            let zero: i32 = 0;
            return value / zero;
        }",
        nexa::ErrorCode::NX7103,
    )
}

fn task_handler_failure(
    source_name: &str,
    package_name: &str,
    body: &str,
    code: nexa::ErrorCode,
) -> Result<EngineDiagnosticEvidence, String> {
    let package_id = package_id(package_name)?;
    let source = memory_source(
        source_name,
        package_name,
        "default-enabled",
        "",
        &format!("use host::test;\n{body}"),
    )?;
    let mut engine = task_builder(source, None)
        .require_export::<TaskRun>()
        .build()
        .map_err(string_error)?;
    engine.discover().map_err(string_error)?;
    engine.enable(&package_id).map_err(string_error)?;
    engine
        .call::<TaskRun>(&package_id, &7)
        .expect_err("task handler evidence must fail");
    capture(&engine, code, "NexaEngine::call")
}

fn ordinary_handler_failure(
    source_name: &str,
    package_name: &str,
    body: &str,
    code: nexa::ErrorCode,
) -> Result<EngineDiagnosticEvidence, String> {
    let package_id = package_id(package_name)?;
    let source = memory_source(
        source_name,
        package_name,
        "default-enabled",
        "",
        &format!("use host::test;\n{body}"),
    )?;
    let mut engine = builder(source, None)
        .require_export::<Run>()
        .build()
        .map_err(string_error)?;
    engine.discover().map_err(string_error)?;
    engine.enable(&package_id).map_err(string_error)?;
    engine
        .call::<Run>(&package_id, &7)
        .expect_err("handler evidence must fail");
    capture(&engine, code, "NexaEngine::call")
}

fn migration_rollback() -> Result<EngineDiagnosticEvidence, String> {
    let package_id = package_id("evidence.migration")?;
    let script = Arc::new(RwLock::new(stateful_script(1, 1, false)));
    let source = SharedSource {
        id: source_id("evidence-migration")?,
        policy: policy([ActivationPolicy::DefaultEnabled]),
        manifest: manifest(package_id.as_str(), "default-enabled", ""),
        script: Arc::clone(&script),
    };
    let mut engine = builder(source, None)
        .require_export::<Run>()
        .build()
        .map_err(string_error)?;
    engine.discover().map_err(string_error)?;
    engine.enable(&package_id).map_err(string_error)?;
    engine
        .set_state_i32(&package_id, "store", "Store", 1, "value", 7)
        .map_err(string_error)?;
    *script.write().expect("source evidence lock") = stateful_script(2, 2, false);
    engine
        .reload(&package_id)
        .expect_err("missing migration must roll back");
    if engine.status(&package_id) != Some(crate::PackageStatus::Enabled) {
        return Err("migration rollback did not retain the active Package".into());
    }
    capture(&engine, nexa::ErrorCode::NX7201, "NexaEngine::reload")
}

fn activation_fault() -> Result<EngineDiagnosticEvidence, String> {
    let package_id = package_id("evidence.activation")?;
    let script = Arc::new(RwLock::new(stateful_script(1, 1, false)));
    let source = SharedSource {
        id: source_id("evidence-activation")?,
        policy: policy([ActivationPolicy::DefaultEnabled]),
        manifest: manifest(package_id.as_str(), "default-enabled", ""),
        script: Arc::clone(&script),
    };
    let mut engine = builder(source, None)
        .require_export::<Run>()
        .build()
        .map_err(string_error)?;
    engine.discover().map_err(string_error)?;
    engine.enable(&package_id).map_err(string_error)?;
    *script.write().expect("source evidence lock") = stateful_script(1, 2, true);
    let activation_error = engine
        .reload(&package_id)
        .expect_err("invalid activation entry must fault after commit");
    if engine.status(&package_id) != Some(crate::PackageStatus::Enabled) {
        return Err(format!(
            "activation failure did not retain the active Package: status={:?}, error={activation_error}",
            engine.status(&package_id)
        ));
    }
    let active_value = engine
        .call::<Run>(&package_id, &7)
        .map_err(|error| format!("last-known-good Package was not callable: {error}"))?
        .value;
    if active_value != 8 {
        return Err(format!(
            "activation failure changed last-known-good behavior: value={active_value}, error={activation_error}"
        ));
    }
    capture(&engine, nexa::ErrorCode::NX7202, "NexaEngine::reload")
}

fn persistence_failure() -> Result<EngineDiagnosticEvidence, String> {
    let package_id = package_id("evidence.persistence")?;
    let source = memory_source(
        "evidence-persistence",
        package_id.as_str(),
        "user-controlled",
        "",
        &valid_script(1),
    )?;
    let root = evidence_temp("persistence");
    remove_if_present(&root)?;
    let storage = root.join("storage");
    let mut engine = builder(source, None)
        .require_export::<Run>()
        .storage_dir(&storage)
        .build()
        .map_err(string_error)?;
    std::fs::create_dir_all(&root).map_err(string_error)?;
    std::fs::write(&storage, "storage path is intentionally a file").map_err(string_error)?;
    engine.discover().map_err(string_error)?;
    engine
        .enable(&package_id)
        .expect_err("persistence path must be unwritable as a directory");
    let evidence = capture(
        &engine,
        nexa::ErrorCode::NX7302,
        "NexaEngine::enable/persist_selections",
    );
    drop(engine);
    remove_if_present(&root)?;
    evidence
}

fn shutdown_failure() -> Result<EngineDiagnosticEvidence, String> {
    let runtime_host = nexa::prelude::RuntimeHost::new(32);
    let contract_runtime_id = contract().contract_runtime_id();
    let authority = host_function_authority();
    let external_realm = nexa::prelude::RealmRuntime::hosted(
        nexa::prelude::RealmConfig {
            realm_id: 9_001,
            ..nexa::prelude::RealmConfig::default()
        },
        runtime_host.clone(),
        Box::new(Registry {
            contract_runtime_id,
            authority,
            held_completion: None,
        }),
    )
    .map_err(string_error)?;
    let source = MemorySource::new(
        source_id("evidence-shutdown")?,
        policy([ActivationPolicy::UserControlled]),
    );
    let mut engine = builder(source, None)
        .runtime_host_for_evidence(runtime_host)
        .build()
        .map_err(string_error)?;
    engine
        .shutdown()
        .expect_err("an externally live Realm must prevent RuntimeHost close");
    let evidence = capture(&engine, nexa::ErrorCode::NX7303, "NexaEngine::shutdown");
    drop(external_realm);
    engine.shutdown().map_err(string_error)?;
    evidence
}

fn capture(
    engine: &NexaEngine,
    code: nexa::ErrorCode,
    public_entrypoint: &'static str,
) -> Result<EngineDiagnosticEvidence, String> {
    let diagnostics = engine.diagnostics();
    let diagnostic = diagnostics
        .iter()
        .rev()
        .find(|diagnostic| diagnostic.diagnostic.code == code)
        .cloned()
        .ok_or_else(|| {
            format!(
                "{public_entrypoint} did not emit {code}; observed {:?}",
                engine
                    .diagnostics()
                    .iter()
                    .map(|diagnostic| {
                        (
                            diagnostic.diagnostic.code.as_str(),
                            diagnostic.diagnostic.message.to_string(),
                        )
                    })
                    .collect::<Vec<_>>()
            )
        })?;
    let human = DiagnosticRenderer::human(&diagnostic);
    let json = DiagnosticRenderer::json(&diagnostic).map_err(string_error)?;
    let ndjson = DiagnosticRenderer::ndjson([&diagnostic]).map_err(string_error)?;
    let real_engine_path = diagnostic.sequence != 0
        && diagnostics
            .iter()
            .filter(|observed| observed.sequence == diagnostic.sequence)
            .count()
            == 1;
    let mut evidence = EngineDiagnosticEvidence {
        code,
        public_entrypoint,
        diagnostic,
        human,
        json,
        ndjson,
        rendering: EngineRenderEvidence::default(),
        observation: EngineDiagnosticObservation {
            real_engine_path,
            ..EngineDiagnosticObservation::default()
        },
    };
    let rendered = observe_rendering(&evidence);
    evidence.rendering = rendered;
    evidence.observation.source_evidence_valid = source_evidence_valid(&evidence.diagnostic);
    Ok(evidence)
}

fn builder(
    source: impl PackageSource + 'static,
    held_completion: Option<Arc<Mutex<Option<HostCompletionTicket>>>>,
) -> crate::NexaEngineBuilder {
    builder_with_contract(source, held_completion, contract())
}

fn task_builder(
    source: impl PackageSource + 'static,
    held_completion: Option<Arc<Mutex<Option<HostCompletionTicket>>>>,
) -> crate::NexaEngineBuilder {
    builder_with_contract(source, held_completion, task_contract())
}

fn builder_with_contract(
    source: impl PackageSource + 'static,
    held_completion: Option<Arc<Mutex<Option<HostCompletionTicket>>>>,
    contract: crate::HostContract,
) -> crate::NexaEngineBuilder {
    let contract_runtime_id = contract.contract_runtime_id();
    let authority = host_function_authority();
    NexaEngine::builder(contract)
        .host_factory(move |_: &crate::PackageContext| {
            Box::new(Registry {
                contract_runtime_id,
                authority: authority.clone(),
                held_completion: held_completion.clone(),
            }) as Box<dyn HostRegistry>
        })
        .package_source(source)
}

fn host_function_authority() -> HostFunctionAuthority {
    EVIDENCE_HOST_AUTHORITY
        .get_or_init(|| {
            let contract = nexa::parse_contract(IDL_SOURCE).expect("diagnostic evidence NIDL is valid");
            let model = nexa::BindingModel::from_contract(&contract)
                .expect("diagnostic evidence Contract runtime binding model");
            let function = model
                .host_functions
                .iter()
                .find(|function| function.identity.source_name == "wait")
                .expect("diagnostic evidence Host wait function");
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

fn contract() -> crate::HostContract {
    *EVIDENCE_CONTRACT.get_or_init(|| {
        let idl = nexa::parse_contract(IDL_SOURCE).expect("diagnostic evidence NIDL is valid");
        for (name, expected) in [
            (Run::NAME, Run::STABLE_ID),
            (MissingRun::NAME, MissingRun::STABLE_ID),
        ] {
            let entrypoint = idl
                .nexa_functions
                .iter()
                .find(|function| function.name == name)
                .expect("diagnostic evidence entrypoint is declared");
            assert_eq!(nexa::entrypoint_stable_id(entrypoint), expected);
        }
        let descriptor = nexa::abi_descriptor(&idl);
        let fingerprint = descriptor.fingerprint.into_bytes();
        let canonical_descriptor = Box::leak(descriptor.bytes.into_boxed_slice());
        crate::HostContract::new(
            "Test",
            IDL_SOURCE,
            canonical_descriptor,
            fingerprint,
            nexa::contract_runtime_id(&idl),
            nexa::HOST_CONTRACT_SCHEMA_VERSION,
        )
    })
}

fn task_contract() -> crate::HostContract {
    *EVIDENCE_TASK_CONTRACT.get_or_init(|| {
        let idl =
            nexa::parse_contract(TASK_IDL_SOURCE).expect("diagnostic evidence task NIDL is valid");
        let entrypoint = idl
            .nexa_functions
            .iter()
            .find(|function| function.name == TaskRun::NAME)
            .expect("diagnostic evidence task entrypoint is declared");
        assert_eq!(nexa::entrypoint_stable_id(entrypoint), TaskRun::STABLE_ID);
        assert!(entrypoint.is_async);
        let descriptor = nexa::abi_descriptor(&idl);
        let fingerprint = descriptor.fingerprint.into_bytes();
        let canonical_descriptor = Box::leak(descriptor.bytes.into_boxed_slice());
        crate::HostContract::new(
            "Test",
            TASK_IDL_SOURCE,
            canonical_descriptor,
            fingerprint,
            nexa::contract_runtime_id(&idl),
            nexa::HOST_CONTRACT_SCHEMA_VERSION,
        )
    })
}

fn policy(activation: impl IntoIterator<Item = ActivationPolicy>) -> PackagePolicy {
    PackagePolicy {
        trust: TrustLevel::Trusted,
        capability_ceiling: CapabilitySet::default(),
        allowed_activation: ActivationSet::new(activation),
        max_packages: 16,
        runtime_limits: PackageRuntimeLimits::default(),
        allow_entitlement: true,
    }
}

fn manifest(id: &str, activation: &str, entitlement: &str) -> String {
    let entitlement = if entitlement.is_empty() {
        String::new()
    } else {
        format!("entitlement = \"{entitlement}\"\n")
    };
    format!(
        "schema = 2\n\
         kind = \"application\"\n\
         id = \"{id}\"\n\
         name = \"Evidence\"\n\
         version = \"1.0.0\"\n\
         source_root = \"src\"\n\
         entry = \"evidence.valid\"\n\
         activation = \"{activation}\"\n\
         handler_fuel = 20000\n\
         capabilities = []\n\
         {entitlement}"
    )
}

fn memory_source(
    source: &str,
    package: &str,
    activation: &str,
    entitlement: &str,
    script: &str,
) -> Result<MemorySource, String> {
    Ok(MemorySource::new(
        source_id(source)?,
        policy([
            ActivationPolicy::Required,
            ActivationPolicy::DefaultEnabled,
            ActivationPolicy::UserControlled,
        ]),
    )
    .package(
        MemoryPackage::new(package, {
            let module = EVIDENCE_ENTRY;
            manifest(package, activation, entitlement).replace(
                "entry = \"evidence.valid\"",
                &format!("entry = \"{module}\""),
            )
        })
        .source(module_source_path(EVIDENCE_ENTRY), script),
    ))
}

fn valid_script(increment: i32) -> String {
    format!(
        "use host::test;\n\
         pub fn run(value: i32) -> i32 {{ return value + {increment}; }}"
    )
}

fn stateful_script(schema: u32, increment: i32, activation_fault: bool) -> String {
    format!(
        "use host::test;\n\
         @state(version = {schema}) class Store {{ mut value: i32,{} }}\n\
         pub fn run(value: i32) -> i32 {{ return value + {increment}; }}\n\
         {}",
        if schema == 1 { "" } else { " mut extra: i32," },
        if activation_fault {
            "@activation pub fn activate() -> i32 { let zero: i32 = 0; return 1 / zero; }"
        } else {
            ""
        }
    )
}

fn module_source_path(module: &str) -> String {
    format!("src/{}.nexa", module.replace('.', "/"))
}

fn package_id(value: &str) -> Result<PackageId, String> {
    PackageId::new(value).map_err(string_error)
}

fn source_id(value: &str) -> Result<SourceId, String> {
    SourceId::new(value).map_err(string_error)
}

fn evidence_temp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nexa-m4-engine-evidence-{label}-{}",
        std::process::id()
    ))
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    if path.exists() {
        if path.is_dir() {
            std::fs::remove_dir_all(path).map_err(string_error)?;
        } else {
            std::fs::remove_file(path).map_err(string_error)?;
        }
    }
    Ok(())
}

fn validate_fixture(root: &Path, code: nexa::ErrorCode) -> Result<(), String> {
    let fixture = load_fixture(root, code)?;
    if fixture.version != 1
        || fixture.pipeline != "engine"
        || fixture.code != code.as_str()
        || normalized(&fixture.expected.message_contains).is_empty()
    {
        return Err(format!("{code} has invalid Engine fixture metadata"));
    }
    Ok(())
}

fn load_fixture(root: &Path, code: nexa::ErrorCode) -> Result<EngineFixture, String> {
    let path = root
        .join("fixtures/diagnostics/cases")
        .join(format!("{code}.json"));
    serde_json::from_slice(
        &std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?,
    )
    .map_err(|error| format!("{}: {error}", path.display()))
}

fn deterministically_equivalent(
    left: &EngineDiagnosticEvidence,
    right: &EngineDiagnosticEvidence,
) -> bool {
    left.code == right.code
        && left.public_entrypoint == right.public_entrypoint
        && left.diagnostic.stage == right.diagnostic.stage
        && left.diagnostic.diagnostic.message == right.diagnostic.diagnostic.message
        && left.diagnostic.diagnostic.primary == right.diagnostic.diagnostic.primary
        && left.diagnostic.diagnostic.secondary == right.diagnostic.diagnostic.secondary
        && left.diagnostic.file == right.diagnostic.file
        && left.diagnostic.related == right.diagnostic.related
        && left.diagnostic.context == right.diagnostic.context
        && left.human == right.human
        && left.json == right.json
        && left.ndjson == right.ndjson
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ObservedPrimary {
    text: String,
    start: u32,
    end: u32,
}

fn observed_primary(item: &EngineDiagnosticEvidence) -> Result<ObservedPrimary, String> {
    let diagnostic = &item.diagnostic;
    let Some(primary) = diagnostic.diagnostic.primary.as_ref() else {
        if diagnostic.file.is_some() {
            return Err(format!(
                "{} has a primary file without a primary label",
                item.code
            ));
        }
        return Ok(ObservedPrimary::default());
    };
    let file = diagnostic.file.as_ref().ok_or_else(|| {
        format!(
            "{} has a primary label without a source identity",
            item.code
        )
    })?;
    let source = diagnostic
        .source_by_identity(file)
        .ok_or_else(|| format!("{} primary source {file} is unavailable", item.code))?;
    let start = usize::try_from(primary.span.start).map_err(string_error)?;
    let end = usize::try_from(primary.span.end).map_err(string_error)?;
    let text = source
        .text()
        .get(start..end)
        .ok_or_else(|| format!("{} primary span {start}..{end} is invalid", item.code))?;
    Ok(ObservedPrimary {
        text: text.to_owned(),
        start: primary.span.start,
        end: primary.span.end,
    })
}

fn observe_rendering(item: &EngineDiagnosticEvidence) -> EngineRenderEvidence {
    let expected = rendered_from_diagnostic(&item.diagnostic);
    let human = human_matches_diagnostic(&item.diagnostic, &item.human);
    let json = serde_json::from_str::<RenderedDiagnostic>(&item.json)
        .is_ok_and(|observed| observed == expected);
    let mut lines = item.ndjson.lines();
    let ndjson = lines
        .next()
        .and_then(|line| serde_json::from_str::<RenderedDiagnostic>(line).ok())
        .is_some_and(|observed| observed == expected)
        && lines.next().is_none()
        && item.ndjson.ends_with('\n');
    EngineRenderEvidence {
        human_matches_diagnostic: human,
        json_matches_diagnostic: json,
        ndjson_matches_diagnostic: ndjson,
    }
}

fn rendered_from_diagnostic(diagnostic: &EngineDiagnostic) -> RenderedDiagnostic {
    RenderedDiagnostic {
        schema: nexa::RENDER_SCHEMA_VERSION,
        position_encoding: nexa::MACHINE_POSITION_ENCODING.to_owned(),
        sequence: diagnostic.sequence,
        code: diagnostic.diagnostic.code.to_string(),
        severity: diagnostic.diagnostic.severity.as_str().to_owned(),
        file: diagnostic
            .file
            .as_ref()
            .map(|identity| identity.path().to_owned()),
        source_identity: diagnostic.file.as_ref().map(ToString::to_string),
        range: rendered_range(
            diagnostic,
            diagnostic.file.as_ref(),
            diagnostic
                .diagnostic
                .primary
                .as_ref()
                .map(|label| label.span),
        ),
        message: diagnostic.diagnostic.message.to_string(),
        related: diagnostic
            .related
            .iter()
            .map(|related| RenderedRelated {
                message: related.message.clone(),
                file: related
                    .file
                    .as_ref()
                    .map(|identity| identity.path().to_owned()),
                source_identity: related.file.as_ref().map(ToString::to_string),
                range: rendered_range(diagnostic, related.file.as_ref(), related.span),
            })
            .collect(),
    }
}

fn rendered_range(
    diagnostic: &EngineDiagnostic,
    identity: Option<&nexa::SourceIdentity>,
    span: Option<nexa::prelude::SourceSpan>,
) -> Option<RenderedRange> {
    let identity = identity?;
    let span = span?;
    let source = diagnostic.source_by_identity(identity)?;
    let range = source.utf16_range(nexa::ByteRange::new(span.start, span.end));
    Some(RenderedRange {
        start: RenderedPosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: RenderedPosition {
            line: range.end.line,
            character: range.end.character,
        },
    })
}

fn human_matches_diagnostic(diagnostic: &EngineDiagnostic, rendered: &str) -> bool {
    let prefix = format!(
        "{}[{}] {:?}: {}",
        diagnostic.diagnostic.severity.as_str(),
        diagnostic.diagnostic.code,
        diagnostic.stage,
        diagnostic.diagnostic.message
    );
    if !rendered.starts_with(&prefix) {
        return false;
    }
    if let Some(package) = &diagnostic.package_id
        && !rendered.contains(&format!("\nPackage: {package}"))
    {
        return false;
    }
    if let Some(primary) = &diagnostic.diagnostic.primary {
        let Some(file) = &diagnostic.file else {
            return false;
        };
        let Some(source) = diagnostic.source_by_identity(file) else {
            return false;
        };
        let position = source.human_position(primary.span.start as usize);
        let location = format!("\n{file}:{}:{}", position.line, position.column);
        if !rendered.contains(&location) || !rendered.contains(&format!("\n  {}", primary.message))
        {
            return false;
        }
        if let Some(line) = source.line_text(position.line)
            && !rendered.contains(&format!("\n  {line}"))
        {
            return false;
        }
    }
    diagnostic
        .related
        .iter()
        .all(|related| rendered.contains(&format!("\nrelated: {}", related.message)))
}

fn source_evidence_valid(diagnostic: &EngineDiagnostic) -> bool {
    let primary_valid = match (
        diagnostic.diagnostic.primary.as_ref(),
        diagnostic.file.as_ref(),
    ) {
        (None, None) => true,
        (Some(primary), Some(file)) => {
            !primary.message.to_string().is_empty()
                && diagnostic.source_identity(primary.span.file) == Some(file)
                && valid_source_span(diagnostic, file, primary.span)
        }
        _ => false,
    };
    primary_valid
        && diagnostic.related.iter().all(|related| {
            if related.message.is_empty() {
                return false;
            }
            match (related.file.as_ref(), related.span) {
                (None, None) => true,
                (Some(file), Some(span)) => valid_source_span(diagnostic, file, span),
                _ => false,
            }
        })
}

fn valid_source_span(
    diagnostic: &EngineDiagnostic,
    identity: &nexa::SourceIdentity,
    span: nexa::prelude::SourceSpan,
) -> bool {
    let Some(source) = diagnostic.source_by_identity(identity) else {
        return false;
    };
    let Ok(start) = usize::try_from(span.start) else {
        return false;
    };
    let Ok(end) = usize::try_from(span.end) else {
        return false;
    };
    start < end && source.text().get(start..end).is_some()
}

fn direct_diagnostic_construction_count(root: &Path) -> Result<usize, String> {
    let path = root.join("crates/nexa-embed/src/diagnostic_evidence.rs");
    let source =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(count_direct_diagnostic_construction(&source))
}

fn count_direct_diagnostic_construction(source: &str) -> usize {
    let tokens = rust_code_tokens(source);
    let constructors = [
        "without_source",
        "from_leaf",
        "from_package_snapshot",
        "from_leaf_diagnostic",
        "from_package_leaf_diagnostic",
        "from_diagnostic_batch",
    ];
    let calls = tokens
        .windows(5)
        .filter(|tokens| {
            matches!(tokens[0].as_str(), "Diagnostic" | "EngineDiagnostic")
                && tokens[1] == ":"
                && tokens[2] == ":"
                && constructors.contains(&tokens[3].as_str())
                && tokens[4] == "("
        })
        .count();
    let literals = tokens
        .windows(2)
        .filter(|tokens| tokens[0] == "EngineDiagnostic" && tokens[1] == "{")
        .count();
    calls + literals
}

fn rust_code_tokens(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
        } else if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
        } else if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            let mut depth = 1_u32;
            while index < bytes.len() && depth != 0 {
                if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                    depth += 1;
                    index += 2;
                } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
        } else if bytes[index] == b'"' {
            index = skip_quoted(bytes, index, b'"');
        } else if bytes[index] == b'\'' {
            if is_rust_char_literal(bytes, index) {
                index = skip_quoted(bytes, index, b'\'');
            } else {
                tokens.push("'".to_owned());
                index += 1;
            }
        } else if bytes[index] == b'r' && matches!(bytes.get(index + 1), Some(b'"' | b'#')) {
            index = skip_raw_string(bytes, index);
        } else if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            tokens.push(source[start..index].to_owned());
        } else {
            tokens.push(char::from(bytes[index]).to_string());
            index += 1;
        }
    }
    tokens
}

fn is_rust_char_literal(bytes: &[u8], index: usize) -> bool {
    if bytes.get(index + 1) == Some(&b'\\') {
        return bytes
            .get(index + 2..)
            .and_then(|tail| tail.iter().position(|byte| *byte == b'\''))
            .is_some_and(|closing| closing <= 9);
    }
    bytes.get(index + 2) == Some(&b'\'')
}

fn skip_quoted(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else {
            let found = bytes[index] == quote;
            index += 1;
            if found {
                break;
            }
        }
    }
    index
}

fn skip_raw_string(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    let mut hashes = 0;
    while bytes.get(index) == Some(&b'#') {
        hashes += 1;
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return index;
    }
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'"'
            && bytes
                .get(index + 1..index + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return index + 1 + hashes;
        }
        index += 1;
    }
    index
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn string_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        count_direct_diagnostic_construction, deterministically_equivalent, observe_rendering,
        source_failure,
    };

    #[test]
    fn construction_scan_observes_code_but_ignores_comments_and_strings() {
        let constructor = concat!("EngineDiagnostic", "::without_source(None)");
        let source = format!(
            "// {constructor}\nlet text = r#\"{constructor}\"#;\n{constructor};\nEngineDiagnostic {{ sequence: 1 }}"
        );
        assert_eq!(count_direct_diagnostic_construction(&source), 2);
    }

    #[test]
    fn render_observation_rejects_copied_or_tampered_output() {
        let mut evidence = source_failure().expect("source failure evidence");
        assert!(observe_rendering(&evidence).json_matches_diagnostic);
        evidence.json = evidence.json.replacen(evidence.code.as_str(), "NX0000", 1);
        assert!(!observe_rendering(&evidence).json_matches_diagnostic);
    }

    #[test]
    fn deterministic_observation_compares_rendered_evidence() {
        let first = source_failure().expect("first source failure evidence");
        let mut second = source_failure().expect("second source failure evidence");
        assert!(deterministically_equivalent(&first, &second));
        second.human.push_str("\nchanged");
        assert!(!deterministically_equivalent(&first, &second));
    }
}
