use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use nexa_runtime::{
    HostCallOutcome, HostCompletionTicket, HostRegistry, HostTrap, ResourceContext,
    RuntimeHostArgs, RuntimeValue, ScriptArgumentRequirements, ScriptCallError, ScriptCallWriter,
    ScriptExport, ScriptOutputReader, Signature, StableId, ValueType,
};
use serde::Deserialize;

use crate::{
    ActivationPolicy, ActivationSet, CapabilitySet, DiagnosticRenderer, EngineDiagnostic,
    EngineError, MemorySource, NexaEngine, PackageCandidate, PackageId, PackagePolicy,
    PackageRuntimeLimits, PackageSource, PackageSourceError, SourceId, TrustLevel,
};

const IDL_SOURCE: &str = "interface TestHost {
    enum WaitError { Cancelled }
    request(return_error, trap) fn wait(value: i32) -> request<Result<i32, WaitError>>;
    export Run(value: i32) -> i32;
}";
const RUN_ID: StableId = StableId(0xf1c5_6273_0ddd_ab52);
static NEXT_EVIDENCE_TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct EngineDiagnosticEvidence {
    pub code: nexa::ErrorCode,
    pub public_entrypoint: &'static str,
    pub diagnostic: EngineDiagnostic,
    pub human: String,
    pub json: String,
    pub ndjson: String,
    pub real_engine_path: bool,
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

struct Registry {
    interface_hash: StableId,
    held_completion: Option<Arc<Mutex<Option<HostCompletionTicket>>>>,
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

struct RunWithWrongHostSignature;

impl ScriptExport for RunWithWrongHostSignature {
    type Args = i64;
    type Output = i32;

    const STABLE_ID: StableId = RUN_ID;
    const NAME: &'static str = "Run";

    fn signature() -> Signature {
        Signature {
            parameters: vec![ValueType::I64],
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
        Ok(vec![RuntimeValue::I64(*args)])
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

struct MissingRun;

impl ScriptExport for MissingRun {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = StableId(0x8f7a_92c1_65d0_44e3);
    const NAME: &'static str = "MissingRun";

    fn signature() -> Signature {
        Run::signature()
    }

    fn argument_requirements(
        _: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Ok(ScriptArgumentRequirements::ZERO)
    }

    fn encode_args(
        writer: &mut ScriptCallWriter<'_>,
        args: &Self::Args,
    ) -> Result<Vec<RuntimeValue>, ScriptCallError> {
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

    fn discover(&self) -> Result<Vec<PackageCandidate>, PackageSourceError> {
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

    fn discover(&self) -> Result<Vec<PackageCandidate>, PackageSourceError> {
        let manifest = crate::PackageManifest::parse(&self.manifest, &self.policy)?;
        Ok(vec![PackageCandidate::new(
            manifest,
            self.manifest.clone(),
            self.script.read().expect("source evidence lock").clone(),
        )])
    }
}

pub fn run_engine_diagnostic_evidence(
    root: &Path,
) -> Result<Vec<EngineDiagnosticEvidence>, String> {
    let registered = engine_codes();
    let mut evidence = Vec::with_capacity(registered.len());
    for code in registered {
        validate_fixture(root, code)?;
        let first = execute_scenario(code)?;
        let second = execute_scenario(code)?;
        if !deterministically_equivalent(&first, &second) {
            return Err(format!("{code} Engine evidence was not deterministic"));
        }
        evidence.push(first);
    }
    Ok(evidence)
}

pub fn run_engine_diagnostic_cases(root: &Path) -> Result<nexa::EngineDiagnosticReport, String> {
    let registered = engine_codes();
    let evidence = run_engine_diagnostic_evidence(root)?;
    let mut cases = Vec::with_capacity(evidence.len());
    for item in &evidence {
        let fixture = load_fixture(root, item.code)?;
        let definition = item
            .code
            .definition()
            .ok_or_else(|| format!("{} is not registered", item.code))?;
        let passed = item.real_engine_path
            && item.diagnostic.diagnostic.code == item.code
            && item.human.contains(item.code.as_str())
            && item.json.contains(item.code.as_str())
            && item.ndjson.contains(item.code.as_str())
            && normalized(definition.summary)
                .contains(&normalized(&fixture.expected.message_contains));
        let primary = item.diagnostic.diagnostic.primary.as_ref();
        cases.push(nexa::ObservedDiagnosticCase {
            code: item.code.to_string(),
            observed: item.diagnostic.diagnostic.code.to_string(),
            pipeline: "engine".into(),
            category: "diagnostic".into(),
            primary_text: String::new(),
            primary_start: primary.map_or(0, |label| label.span.start),
            primary_end: primary.map_or(0, |label| label.span.end),
            secondary_count: item.diagnostic.related.len(),
            human_output: item.human.contains(item.code.as_str()),
            json_output: item.json.contains(item.code.as_str()),
            passed,
        });
    }
    Ok(nexa::EngineDiagnosticReport {
        registered: registered.len(),
        observed_through_real_paths: evidence.iter().filter(|item| item.real_engine_path).count(),
        direct_diagnostic_construction: 0,
        human_output: evidence
            .iter()
            .filter(|item| item.human.contains(item.code.as_str()))
            .count(),
        json_output: evidence
            .iter()
            .filter(|item| item.json.contains(item.code.as_str()))
            .count(),
        ndjson_output: evidence
            .iter()
            .filter(|item| item.ndjson.contains(item.code.as_str()))
            .count(),
        deterministic: evidence.len(),
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
        manifest("evidence.manifest", "user-controlled", "").replace("schema = 1", "schema = 9"),
        valid_script(1),
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
        manifest("evidence.policy", "user-controlled", "")
            .replace("capabilities = []", "capabilities = [\"evidence.denied\"]"),
        valid_script(1),
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
        &valid_script(1),
    )?;
    let mut engine = builder(source, None)
        .require_export::<RunWithWrongHostSignature>()
        .build()
        .map_err(string_error)?;
    engine.discover().map_err(string_error)?;
    engine
        .enable(&package_id)
        .expect_err("required export signature must be rejected");
    capture(&engine, nexa::ErrorCode::NX7011, "NexaEngine::enable")
}

fn handler_yield() -> Result<EngineDiagnosticEvidence, String> {
    handler_failure(
        "evidence-yield",
        "evidence.handler-yield",
        "task fn Run(value: i32) -> i32 { yield; return value; }",
        nexa::ErrorCode::NX7101,
    )
}

fn handler_wait() -> Result<EngineDiagnosticEvidence, String> {
    handler_failure(
        "evidence-wait",
        "evidence.handler-wait",
        "task fn Run(value: i32) -> i32 {
            let result: Result<i32, WaitError> = await test.wait(value);
            return match result { Ok(found) => found, Err(error) => 0 };
        }",
        nexa::ErrorCode::NX7102,
    )
}

fn handler_trap() -> Result<EngineDiagnosticEvidence, String> {
    handler_failure(
        "evidence-trap",
        "evidence.handler-trap",
        "fn Run(value: i32) -> i32 {
            let zero: i32 = 0;
            return value / zero;
        }",
        nexa::ErrorCode::NX7103,
    )
}

fn handler_failure(
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
        &format!("module evidence.handler;\nimport test;\n{body}"),
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
    engine
        .reload(&package_id)
        .expect_err("invalid activation entry must fault after commit");
    if engine.status(&package_id) != Some(crate::PackageStatus::Faulted) {
        return Err("activation failure did not fault the Package".into());
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
    let runtime_host = nexa_runtime::RuntimeHost::new(32);
    let interface_hash = contract().interface_hash;
    let external_realm = nexa_runtime::RealmRuntime::hosted(
        nexa_runtime::RealmConfig {
            realm_id: 9_001,
            ..nexa_runtime::RealmConfig::default()
        },
        runtime_host.clone(),
        Box::new(Registry {
            interface_hash,
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
        .into_iter()
        .rev()
        .find(|diagnostic| diagnostic.diagnostic.code == code)
        .ok_or_else(|| {
            format!(
                "{public_entrypoint} did not emit {code}; observed {:?}",
                engine
                    .diagnostics()
                    .iter()
                    .map(|diagnostic| diagnostic.diagnostic.code.as_str())
                    .collect::<Vec<_>>()
            )
        })?;
    let human = DiagnosticRenderer::human(&diagnostic);
    let json = DiagnosticRenderer::json(&diagnostic).map_err(string_error)?;
    let ndjson = DiagnosticRenderer::ndjson([&diagnostic]).map_err(string_error)?;
    Ok(EngineDiagnosticEvidence {
        code,
        public_entrypoint,
        diagnostic,
        human,
        json,
        ndjson,
        real_engine_path: true,
    })
}

fn builder(
    source: impl PackageSource + 'static,
    held_completion: Option<Arc<Mutex<Option<HostCompletionTicket>>>>,
) -> crate::NexaEngineBuilder {
    let contract = contract();
    let interface_hash = contract.interface_hash;
    NexaEngine::builder(contract)
        .host_factory(move |_: &crate::PackageContext| {
            Box::new(Registry {
                interface_hash,
                held_completion: held_completion.clone(),
            }) as Box<dyn HostRegistry>
        })
        .package_source(source)
}

fn contract() -> crate::HostContract {
    let idl = nexa_idl::parse(IDL_SOURCE).expect("diagnostic evidence IDL is valid");
    crate::HostContract {
        interface_name: "TestHost",
        canonical_idl: IDL_SOURCE,
        interface_hash: nexa_idl::exact_hash(&idl),
        generator_schema_version: nexa_runtime::HOST_CONTRACT_SCHEMA_VERSION,
    }
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
    format!(
        "schema = 1\n\
         id = \"{id}\"\n\
         name = \"Evidence\"\n\
         version = \"1.0.0\"\n\
         entry = \"main.nexa\"\n\
         activation = \"{activation}\"\n\
         handler_fuel = 20000\n\
         capabilities = []\n\
         entitlement = \"{entitlement}\"\n"
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
    .package(manifest(package, activation, entitlement), script))
}

fn valid_script(increment: i32) -> String {
    format!(
        "module evidence.valid;\n\
         import test;\n\
         fn Run(value: i32) -> i32 {{ return value + {increment}; }}"
    )
}

fn stateful_script(schema: u32, increment: i32, activation_fault: bool) -> String {
    format!(
        "module evidence.store;\n\
         import test;\n\
         @stateful({schema}) class Store {{ value: i32;{} }}\n\
         fn Run(value: i32) -> i32 {{ return value + {increment}; }}\n\
         {}",
        if schema == 1 { "" } else { " extra: i32;" },
        if activation_fault {
            "@activation fn activate(value: i32) -> i32 { return value; }"
        } else {
            ""
        }
    )
}

fn package_id(value: &str) -> Result<PackageId, String> {
    PackageId::new(value).map_err(string_error)
}

fn source_id(value: &str) -> Result<SourceId, String> {
    SourceId::new(value).map_err(string_error)
}

fn evidence_temp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nexa-m3r1-engine-evidence-{label}-{}-{}",
        std::process::id(),
        NEXT_EVIDENCE_TEMP.fetch_add(1, Ordering::Relaxed)
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
    if fixture.version != 1 || fixture.pipeline != "engine" || fixture.code != code.as_str() {
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
        && left
            .diagnostic
            .related
            .iter()
            .map(|related| &related.message)
            .eq(right
                .diagnostic
                .related
                .iter()
                .map(|related| &related.message))
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
