use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use nexa as nexa_idl;
use nexa::prelude as nexa_runtime;
use nexa_embed::{
    ActivationPolicy, ActivationSet, CandidateBuildContext, CandidateTerminalKind, CapabilitySet,
    DevelopmentConfig, DevelopmentEvent, DirectorySource, DiscoveredPackage, EngineError,
    EntitlementId, EntitlementResolver, HostContract, MemoryPackage, MemorySource, NexaEngine,
    PackageId, PackagePolicy, PackageRuntimeLimits, PackageSource, PackageSourceError,
    PackageStatus, SourceId, TrustLevel,
};
use nexa_runtime::{
    FunctionEffect, HostCallOutcome, HostFunctionAuthority, HostRegistry, HostTrap,
    ResourceContext, RuntimeHostArgs, RuntimeValue, ScriptArgumentRequirements, ScriptCallError,
    ScriptCallWriter, ScriptExport, ScriptOutputReader, Signature, StableId, ValueType,
};

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
const TASK_IDL_SOURCE: &str = "contract TestHost {
    enum WaitError { Cancelled, }
    host {
        @cancel(return_error)
        @abandon(trap)
        async fn wait(value: i32) -> Result<i32, WaitError>;
    }
    nexa {
        async fn run(value: i32) -> i32;
    }
}";
const RUN_ID: StableId = StableId(0x8143_9374_8b64_00a6);
const METER_RUN_ID: StableId = StableId(0x39e3_0598_78a5_d67d);

fn host_function_authority(
    contract: &nexa_idl::ValidatedContract,
    name: &str,
) -> HostFunctionAuthority {
    let model = nexa_idl::BindingModel::from_contract(contract)
        .expect("test Host Contract has a runtime binding model");
    let function = model
        .host_functions
        .iter()
        .find(|function| function.identity.source_name == name)
        .expect("test Host function is declared");
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

struct Registry {
    contract_runtime_id: StableId,
    authority: HostFunctionAuthority,
}

impl HostRegistry for Registry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.contract_runtime_id)
    }

    fn function_authority(&self, id: StableId) -> Option<&HostFunctionAuthority> {
        (id == self.authority.stable_id()).then_some(&self.authority)
    }

    fn call_runtime(
        &mut self,
        id: StableId,
        context: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id == self.authority.stable_id() {
            let pending = context
                .create_request()
                .map_err(|_| HostTrap::ResourceCapacity)?;
            Ok(HostCallOutcome::Pending(pending.request))
        } else {
            Err(HostTrap::UnknownFunction(id))
        }
    }
}

struct TrappingRegistry {
    contract_runtime_id: StableId,
    authority: HostFunctionAuthority,
}

impl HostRegistry for TrappingRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.contract_runtime_id)
    }

    fn function_authority(&self, id: StableId) -> Option<&HostFunctionAuthority> {
        (id == self.authority.stable_id()).then_some(&self.authority)
    }

    fn call_runtime(
        &mut self,
        id: StableId,
        _: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id == self.authority.stable_id() {
            Err(HostTrap::Panicked)
        } else {
            Err(HostTrap::UnknownFunction(id))
        }
    }
}

struct MeteredRegistry {
    contract_runtime_id: StableId,
    authority: HostFunctionAuthority,
}

impl HostRegistry for MeteredRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.contract_runtime_id)
    }

    fn function_authority(&self, id: StableId) -> Option<&HostFunctionAuthority> {
        (id == self.authority.stable_id()).then_some(&self.authority)
    }

    fn call_runtime(
        &mut self,
        id: StableId,
        _: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != self.authority.stable_id() {
            return Err(HostTrap::UnknownFunction(id));
        }
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::I32(
            args.i32(0)?.saturating_add(1),
        )))
    }
}

struct Run;

impl ScriptExport for Run {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = RUN_ID;
    const NAME: &'static str = "run";

    fn signature() -> Signature {
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        }
    }

    fn effect() -> FunctionEffect {
        FunctionEffect::Ordinary
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

struct MeterRun;

impl ScriptExport for MeterRun {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = METER_RUN_ID;
    const NAME: &'static str = "run";

    fn signature() -> Signature {
        Run::signature()
    }

    fn effect() -> FunctionEffect {
        FunctionEffect::Ordinary
    }

    fn argument_requirements(
        args: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Run::argument_requirements(args)
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

struct TaskRun;

impl ScriptExport for TaskRun {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = RUN_ID;
    const NAME: &'static str = "run";

    fn signature() -> Signature {
        Run::signature()
    }

    fn effect() -> FunctionEffect {
        FunctionEffect::Task
    }

    fn argument_requirements(
        args: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Run::argument_requirements(args)
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

fn contract() -> HostContract {
    static CONTRACT: OnceLock<HostContract> = OnceLock::new();
    *CONTRACT.get_or_init(|| {
        let idl = nexa_idl::parse_nidl(IDL_SOURCE).expect("test NIDL");
        let run = idl
            .nexa_functions
            .iter()
            .find(|function| function.name == Run::NAME)
            .expect("test NIDL declares the run entrypoint");
        assert_eq!(nexa_idl::entrypoint_stable_id(run), RUN_ID);
        let descriptor = nexa_idl::abi_descriptor(&idl);
        let fingerprint = descriptor.fingerprint.into_bytes();
        let descriptor: &'static [u8] = Box::leak(descriptor.bytes.into_boxed_slice());
        HostContract::new(
            "TestHost",
            IDL_SOURCE,
            descriptor,
            fingerprint,
            nexa_idl::contract_runtime_id(&idl),
            nexa_runtime::HOST_CONTRACT_SCHEMA_VERSION,
        )
    })
}

fn task_contract() -> HostContract {
    static CONTRACT: OnceLock<HostContract> = OnceLock::new();
    *CONTRACT.get_or_init(|| {
        let idl = nexa_idl::parse_nidl(TASK_IDL_SOURCE).expect("test Task NIDL");
        let run = idl
            .nexa_functions
            .iter()
            .find(|function| function.name == TaskRun::NAME)
            .expect("test Task NIDL declares the run entrypoint");
        assert!(run.is_async);
        assert_eq!(nexa_idl::entrypoint_stable_id(run), RUN_ID);
        let descriptor = nexa_idl::abi_descriptor(&idl);
        let fingerprint = descriptor.fingerprint.into_bytes();
        let descriptor: &'static [u8] = Box::leak(descriptor.bytes.into_boxed_slice());
        HostContract::new(
            "TestHost",
            TASK_IDL_SOURCE,
            descriptor,
            fingerprint,
            nexa_idl::contract_runtime_id(&idl),
            nexa_runtime::HOST_CONTRACT_SCHEMA_VERSION,
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
    let entry = module_name(id);
    let entitlement = if entitlement.is_empty() {
        String::new()
    } else {
        format!("entitlement = \"{entitlement}\"\n")
    };
    format!(
        "schema = 2\n\
         kind = \"application\"\n\
         id = \"{id}\"\n\
         name = \"Test\"\n\
         version = \"1.0.0\"\n\
         source_root = \"src\"\n\
         entry = \"{entry}\"\n\
         activation = \"{activation}\"\n\
         handler_fuel = 20000\n\
         capabilities = []\n\
         {entitlement}"
    )
}

fn module_name(package: &str) -> String {
    package.replace('-', "_")
}

fn source_path(module: &str) -> String {
    format!("src/{}.nexa", module.replace('.', "/"))
}

fn source(id: &str, package: &str, activation: &str, script: &str) -> MemorySource {
    MemorySource::new(
        SourceId::new(id).expect("source ID"),
        policy([
            ActivationPolicy::Required,
            ActivationPolicy::DefaultEnabled,
            ActivationPolicy::UserControlled,
        ]),
    )
    .package({
        let module = module_name(package);
        MemoryPackage::new(package.replace('.', "-"), manifest(package, activation, "")).source(
            source_path(&module),
            format!("use host::test_host as test;\n{script}"),
        )
    })
}

fn builder(source: impl PackageSource + 'static) -> nexa_embed::NexaEngineBuilder {
    let contract = contract();
    let hash = contract.contract_runtime_id();
    let idl = nexa_idl::parse_nidl(IDL_SOURCE).expect("test NIDL");
    let authority = host_function_authority(&idl, "wait");
    NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(Registry {
                contract_runtime_id: hash,
                authority: authority.clone(),
            }) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<Run>()
}

fn task_builder(source: impl PackageSource + 'static) -> nexa_embed::NexaEngineBuilder {
    let contract = task_contract();
    let hash = contract.contract_runtime_id();
    let idl = nexa_idl::parse_nidl(TASK_IDL_SOURCE).expect("test Task NIDL");
    let authority = host_function_authority(&idl, "wait");
    NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(Registry {
                contract_runtime_id: hash,
                authority: authority.clone(),
            }) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<TaskRun>()
}

#[test]
fn exact_host_source_identity_and_raw_text_enter_candidate_fingerprint() {
    let discover_fingerprint = |build| {
        source(
            "exact-host-source",
            "tests.exact_host_source",
            "user-controlled",
            "pub fn run(value: i32) -> i32 { return value; }",
        )
        .discover(&build)
        .expect("discover exact Host source")
        .into_iter()
        .next()
        .expect("one application")
        .candidate
        .build_fingerprint
    };
    let raw = format!("\n{IDL_SOURCE}\n");
    let first = discover_fingerprint(CandidateBuildContext::with_source(
        nexa::SourceIdentity::standalone("contracts/first.nidl"),
        raw.as_bytes().to_vec(),
    ));
    let changed_identity = discover_fingerprint(CandidateBuildContext::with_source(
        nexa::SourceIdentity::standalone("contracts/second.nidl"),
        raw.as_bytes().to_vec(),
    ));
    let changed_raw = discover_fingerprint(CandidateBuildContext::with_source(
        nexa::SourceIdentity::standalone("contracts/first.nidl"),
        format!("{IDL_SOURCE}\n\n").into_bytes(),
    ));

    assert_ne!(first, changed_identity);
    assert_ne!(first, changed_raw);
}

#[test]
fn builder_rejects_exact_host_source_parse_mismatch() {
    let result = builder(source(
        "mismatched-host-source",
        "tests.mismatched_host_source",
        "user-controlled",
        "pub fn run(value: i32) -> i32 { return value; }",
    ))
    .host_contract_source(
        nexa::SourceIdentity::standalone("contracts/mismatch.nidl"),
        "contract OtherHost { nexa { fn run(value: i32) -> i32; } }",
    )
    .build();

    assert!(matches!(result, Err(EngineError::Contract(_))));
}

#[test]
fn package_analysis_batch_retains_every_diagnostic_and_one_shared_snapshot() {
    let mut engine = builder(source(
        "analysis-batch",
        "tests.analysis_batch",
        "user-controlled",
        "fn bad_a() -> i32 { return missing_a; }\n\
         fn bad_b() -> i32 { return missing_b; }\n\
         pub fn run(value: i32) -> i32 { return value; }",
    ))
    .build()
    .expect("build");
    engine.discover().expect("discover");
    let package_id = PackageId::new("tests.analysis_batch").expect("Package ID");
    assert!(matches!(
        engine.enable(&package_id),
        Err(EngineError::Diagnostic(_))
    ));
    let diagnostics = engine
        .diagnostics()
        .into_iter()
        .filter(|diagnostic| diagnostic.package_id.as_ref() == Some(&package_id))
        .collect::<Vec<_>>();
    assert!(
        diagnostics.len() >= 2,
        "the Engine truncated a package DiagnosticBatch: {diagnostics:#?}"
    );
    let first = diagnostics[0]
        .source_snapshot
        .as_ref()
        .expect("first package diagnostic source snapshot");
    for diagnostic in &diagnostics[1..] {
        assert!(Arc::ptr_eq(
            first,
            diagnostic
                .source_snapshot
                .as_ref()
                .expect("shared package diagnostic source snapshot"),
        ));
    }
}

#[test]
fn memory_source_enables_calls_disables_and_shuts_down() {
    let mut engine = builder(source(
        "memory",
        "tests.basic",
        "default-enabled",
        "pub fn run(value: i32) -> i32 { return value + 1; }",
    ))
    .build()
    .expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let id = PackageId::new("tests.basic").expect("package ID");
    assert_eq!(engine.call::<Run>(&id, &41).expect("call").value, 42);
    assert_eq!(engine.health().enabled_packages, 1);
    engine.disable(&id).expect("disable");
    assert_eq!(engine.health().enabled_packages, 0);
    assert_eq!(engine.health().host_pending_releases, 0);
    engine.shutdown().expect("shutdown");
}

#[test]
fn engine_records_instruction_count_independently_from_fuel_charge() {
    const METER_IDL: &str = "contract MeterHost {
        host {
            @fuel(11)
            fn expensive(value: i32) -> i32;
        }
        nexa {
            fn run(value: i32) -> i32;
        }
    }";
    let idl = nexa_idl::parse_nidl(METER_IDL).expect("meter NIDL");
    let run = idl
        .nexa_functions
        .iter()
        .find(|function| function.name == MeterRun::NAME)
        .expect("meter NIDL declares the run entrypoint");
    assert_eq!(nexa_idl::entrypoint_stable_id(run), METER_RUN_ID);
    let descriptor = nexa_idl::abi_descriptor(&idl);
    let fingerprint = descriptor.fingerprint.into_bytes();
    let descriptor: &'static [u8] = Box::leak(descriptor.bytes.into_boxed_slice());
    let contract = HostContract::new(
        "MeterHost",
        METER_IDL,
        descriptor,
        fingerprint,
        nexa_idl::contract_runtime_id(&idl),
        nexa_runtime::HOST_CONTRACT_SCHEMA_VERSION,
    );
    let hash = contract.contract_runtime_id();
    let authority = host_function_authority(&idl, "expensive");
    let source = MemorySource::new(
        SourceId::new("meter").expect("source ID"),
        policy([ActivationPolicy::DefaultEnabled]),
    )
    .package(
        MemoryPackage::new(
            "tests-meter",
            manifest("tests.meter", "default-enabled", ""),
        )
        .source(
            "src/tests/meter.nexa",
            "use host::meter_host as meter;
             pub fn run(value: i32) -> i32 { return meter::expensive(value); }",
        ),
    );
    let mut engine = NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(MeteredRegistry {
                contract_runtime_id: hash,
                authority: authority.clone(),
            }) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<MeterRun>()
        .build()
        .expect("meter Engine");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let package_id = PackageId::new("tests.meter").expect("package ID");
    assert_eq!(
        engine
            .call::<MeterRun>(&package_id, &7)
            .expect("metered call")
            .value,
        8
    );
    let inspection = engine.inspection();
    let package = inspection
        .packages
        .iter()
        .find(|package| package.package_id == package_id)
        .expect("metered Package inspection");
    assert!(package.handler_instructions_this_tick > 0);
    assert!(package.fuel_used_this_tick > package.handler_instructions_this_tick);
    engine.shutdown().expect("shutdown");
}

#[test]
fn duplicate_source_and_package_ids_are_rejected_deterministically() {
    let left = source(
        "same",
        "tests.left",
        "user-controlled",
        "pub fn run(value: i32) -> i32 { return value; }",
    );
    let right = source(
        "same",
        "tests.right",
        "user-controlled",
        "pub fn run(value: i32) -> i32 { return value; }",
    );
    assert!(matches!(
        builder(left).package_source(right).build(),
        Err(EngineError::DuplicateSourceId(_))
    ));

    let mut engine = builder(source(
        "left",
        "tests.duplicate",
        "user-controlled",
        "pub fn run(value: i32) -> i32 { return value; }",
    ))
    .package_source(source(
        "right",
        "tests.duplicate",
        "user-controlled",
        "pub fn run(value: i32) -> i32 { return value + 1; }",
    ))
    .build()
    .expect("build duplicate candidates");
    let packages = engine.discover().expect("discover duplicates");
    assert_eq!(packages.len(), 2);
    assert!(
        packages
            .iter()
            .all(|package| package.status == PackageStatus::Incompatible)
    );
}

#[derive(Clone, Default)]
struct SharedEntitlements(Arc<RwLock<Vec<EntitlementId>>>);

impl EntitlementResolver for SharedEntitlements {
    fn contains(&self, id: &EntitlementId) -> bool {
        self.0.read().expect("entitlement lock").contains(id)
    }
}

#[test]
fn entitlement_lock_unlock_and_required_policy_are_enforced() {
    let resolver = SharedEntitlements::default();
    let entitlement = EntitlementId::new("tests.license").expect("entitlement ID");
    let licensed = MemorySource::new(
        SourceId::new("licensed").expect("source ID"),
        policy([ActivationPolicy::UserControlled]),
    )
    .package(
        MemoryPackage::new(
            "tests-licensed",
            manifest("tests.licensed", "user-controlled", "tests.license"),
        )
        .source(
            "src/tests/licensed.nexa",
            "use host::test_host as test;\n\
             pub fn run(value: i32) -> i32 { return value; }",
        ),
    );
    let mut engine = builder(licensed)
        .entitlements(resolver.clone())
        .build()
        .expect("build");
    engine.discover().expect("discover");
    let id = PackageId::new("tests.licensed").expect("package ID");
    assert_eq!(engine.status(&id), Some(PackageStatus::Locked));
    resolver
        .0
        .write()
        .expect("entitlement lock")
        .push(entitlement);
    engine.refresh_entitlements().expect("unlock");
    assert_eq!(engine.status(&id), Some(PackageStatus::Disabled));
    engine.enable(&id).expect("enable unlocked package");

    let mut required = builder(source(
        "required",
        "tests.required",
        "required",
        "pub fn run(value: i32) -> i32 { return value; }",
    ))
    .build()
    .expect("build");
    required.discover().expect("discover");
    required.enable_defaults().expect("enable required");
    let id = PackageId::new("tests.required").expect("package ID");
    assert!(matches!(
        required.disable(&id),
        Err(EngineError::RequiredPackage(_))
    ));
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
        let entry = self
            .manifest
            .lines()
            .find_map(|line| line.trim().strip_prefix("entry = \""))
            .and_then(|value| value.strip_suffix('"'))
            .expect("schema-2 test entry");
        MemorySource::new(self.id.clone(), self.policy.clone())
            .package(
                MemoryPackage::new(self.id.as_str(), self.manifest.clone()).source(
                    source_path(entry),
                    self.script.read().expect("source lock").clone(),
                ),
            )
            .discover(build)
    }
}

#[derive(Clone)]
struct RemovableSource {
    source: SharedSource,
    available: Arc<RwLock<bool>>,
}

impl PackageSource for RemovableSource {
    fn id(&self) -> &SourceId {
        &self.source.id
    }

    fn policy(&self) -> &PackagePolicy {
        &self.source.policy
    }

    fn discover(
        &self,
        build: &CandidateBuildContext,
    ) -> Result<Vec<DiscoveredPackage>, PackageSourceError> {
        if !*self.available.read().expect("availability lock") {
            return Err(PackageSourceError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Package source disappeared",
            )));
        }
        self.source.discover(build)
    }
}

#[test]
fn reload_uses_fresh_source_and_rolls_back_compile_failure() {
    let script = Arc::new(RwLock::new(
        "use host::test_host as test;\n\
         pub fn run(value: i32) -> i32 { return value + 1; }"
            .to_owned(),
    ));
    let source = SharedSource {
        id: SourceId::new("reload").expect("source ID"),
        policy: policy([ActivationPolicy::DefaultEnabled]),
        manifest: manifest("tests.reload", "default-enabled", ""),
        script: script.clone(),
    };
    let mut engine = builder(source).build().expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let id = PackageId::new("tests.reload").expect("package ID");
    assert_eq!(engine.call::<Run>(&id, &1).expect("call v1").value, 2);
    *script.write().expect("source lock") = "use host::test_host as test;\n\
         pub fn run(value: i32) -> i32 { return value + 2; }"
        .into();
    engine.reload(&id).expect("reload v2");
    assert_eq!(engine.call::<Run>(&id, &1).expect("call v2").value, 3);
    *script.write().expect("source lock") = "pub fn run(".into();
    assert!(matches!(
        engine.reload(&id),
        Err(EngineError::Source { .. } | EngineError::Diagnostic(_))
    ));
    assert_eq!(engine.status(&id), Some(PackageStatus::Enabled));
    assert_eq!(
        engine
            .call::<Run>(&id, &1)
            .expect("last known good artifact retained")
            .value,
        3
    );
}

#[test]
fn handler_yield_fault_isolated_and_dispatch_order_is_stable() {
    let mut engine = task_builder(source(
        "normal",
        "tests.a-normal",
        "default-enabled",
        "pub async fn run(value: i32) -> i32 { return value; }",
    ))
    .package_source(source(
        "yielding",
        "tests.z-yield",
        "default-enabled",
        "pub async fn run(value: i32) -> i32 { yield; return value; }",
    ))
    .build()
    .expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let results = engine.dispatch::<TaskRun>(&7);
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0]
            .as_ref()
            .expect("normal output")
            .package_id
            .as_str(),
        "tests.a-normal"
    );
    assert!(matches!(results[1], Err(EngineError::Handler(_, _))));
    assert_eq!(
        engine.status(&PackageId::new("tests.a-normal").expect("package ID")),
        Some(PackageStatus::Enabled)
    );
    assert_eq!(
        engine.status(&PackageId::new("tests.z-yield").expect("package ID")),
        Some(PackageStatus::Faulted)
    );
}

#[test]
fn trap_fuel_yield_and_host_wait_are_distinct_package_failures() {
    for (source_id, package_id, manifest_patch, is_task, script) in [
        (
            "trap",
            "tests.trap",
            "",
            false,
            "pub fn run(value: i32) -> i32 { let zero: i32 = 0; return value / zero; }",
        ),
        (
            "fuel",
            "tests.fuel",
            "handler_fuel = 1",
            false,
            "pub fn run(value: i32) -> i32 { return value + 1; }",
        ),
        (
            "wait",
            "tests.wait",
            "",
            true,
            "pub async fn run(value: i32) -> i32 {
                 let result: Result<i32, test::WaitError> = test::wait(value).await;
                 return match result { Result::Ok(found) => found, Result::Err(error) => 0 };
             }",
        ),
    ] {
        let raw_manifest = manifest(package_id, "default-enabled", "")
            .replace("handler_fuel = 20000", manifest_patch);
        let source = MemorySource::new(
            SourceId::new(source_id).expect("source ID"),
            policy([ActivationPolicy::DefaultEnabled]),
        )
        .package({
            let module = module_name(package_id);
            MemoryPackage::new(package_id.replace('.', "-"), raw_manifest).source(
                source_path(&module),
                format!("use host::test_host as test;\n{script}"),
            )
        });
        let mut engine = if is_task {
            task_builder(source)
        } else {
            builder(source)
        }
        .build()
        .expect("build");
        engine.discover().expect("discover");
        engine.enable_defaults().expect("enable");
        let id = PackageId::new(package_id).expect("package ID");
        let result = if is_task {
            engine.call::<TaskRun>(&id, &7)
        } else {
            engine.call::<Run>(&id, &7)
        };
        assert!(matches!(result, Err(EngineError::Handler(_, _))));
        assert_eq!(engine.status(&id), Some(PackageStatus::Faulted));
        assert_eq!(engine.health().host_pending_releases, 0);
        assert!(
            engine
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.package_id.as_ref() == Some(&id))
        );
    }
}

#[test]
fn runtime_trap_diagnostic_contains_script_stack_and_host_boundary() {
    let source = source(
        "host-trap",
        "tests.host-trap",
        "default-enabled",
        "pub async fn run(value: i32) -> i32 {
             let result: Result<i32, test::WaitError> = test::wait(value).await;
             return match result { Result::Ok(found) => found, Result::Err(error) => 0 };
         }",
    );
    let contract = task_contract();
    let hash = contract.contract_runtime_id();
    let idl = nexa_idl::parse_nidl(TASK_IDL_SOURCE).expect("test Task NIDL");
    let authority = host_function_authority(&idl, "wait");
    let mut engine = NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(TrappingRegistry {
                contract_runtime_id: hash,
                authority: authority.clone(),
            }) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<TaskRun>()
        .build()
        .expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let id = PackageId::new("tests.host-trap").expect("package ID");
    assert!(matches!(
        engine.call::<TaskRun>(&id, &7),
        Err(EngineError::Handler(_, _))
    ));
    let diagnostic = engine
        .diagnostics()
        .into_iter()
        .find(|diagnostic| diagnostic.package_id.as_ref() == Some(&id))
        .expect("runtime diagnostic");
    assert!(diagnostic.file.is_some());
    assert_eq!(diagnostic.context.export.as_deref(), Some("run"));
    assert!(
        diagnostic
            .related
            .iter()
            .any(|related| related.message.contains("at run"))
    );
    assert!(
        diagnostic
            .related
            .iter()
            .any(|related| related.message.contains("while calling Host function"))
    );
    assert!(
        diagnostic
            .related
            .iter()
            .any(|related| related.message.contains("declared here"))
    );
}

#[test]
fn policy_rejections_and_enable_failure_leave_no_runtime() {
    let ceiling = nexa_embed::CapabilityId::new("tests.allowed").expect("capability");
    let policy = PackagePolicy {
        capability_ceiling: CapabilitySet::new([ceiling]),
        allowed_activation: ActivationSet::new([ActivationPolicy::UserControlled]),
        ..policy([ActivationPolicy::UserControlled])
    };
    let excessive = manifest("tests.policy", "user-controlled", "")
        .replace("capabilities = []", "capabilities = [\"tests.denied\"]");
    assert!(matches!(
        MemorySource::new(
            SourceId::new("policy-capability").expect("source ID"),
            policy.clone(),
        )
        .package(MemoryPackage::new("tests-policy", excessive).source(
            "src/tests/policy.nexa",
            "use host::test_host as test;\npub fn run(value: i32) -> i32 { return value; }",
        ),)
        .discover(&CandidateBuildContext::new(IDL_SOURCE.as_bytes().to_vec())),
        Err(PackageSourceError::Policy(
            nexa_embed::ManifestError::CapabilityCeiling
        ))
    ));
    let illegal_activation = manifest("tests.policy", "required", "");
    assert!(matches!(
        MemorySource::new(
            SourceId::new("policy-activation").expect("source ID"),
            policy,
        )
        .package(
            MemoryPackage::new("tests-policy", illegal_activation).source(
                "src/tests/policy.nexa",
                "use host::test_host as test;\npub fn run(value: i32) -> i32 { return value; }",
            ),
        )
        .discover(&CandidateBuildContext::new(IDL_SOURCE.as_bytes().to_vec())),
        Err(PackageSourceError::Policy(
            nexa_embed::ManifestError::ActivationNotAllowed
        ))
    ));

    let mut engine = builder(source(
        "broken",
        "tests.broken",
        "default-enabled",
        "pub fn run(",
    ))
    .build()
    .expect("build");
    engine.discover().expect("discover");
    let id = PackageId::new("tests.broken").expect("package ID");
    assert!(engine.enable(&id).is_err());
    assert_eq!(engine.status(&id), Some(PackageStatus::Faulted));
    assert_eq!(engine.health().enabled_packages, 0);
    assert_eq!(engine.health().host_pending_releases, 0);
}

#[test]
fn change_scan_migration_rollback_and_activation_fault_follow_contract() {
    let script = Arc::new(RwLock::new(
        "use host::test_host as test;\n\
         @state(version = 1) class Store { mut value: i32, }\n\
         pub fn run(value: i32) -> i32 { return value + 1; }"
            .to_owned(),
    ));
    let source = SharedSource {
        id: SourceId::new("state").expect("source ID"),
        policy: policy([ActivationPolicy::DefaultEnabled]),
        manifest: manifest("tests.state", "default-enabled", ""),
        script: script.clone(),
    };
    let mut engine = builder(source).build().expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let id = PackageId::new("tests.state").expect("package ID");
    engine
        .set_state_i32(&id, "store", "Store", 1, "value", 9)
        .expect("insert state");

    *script.write().expect("source lock") = "use host::test_host as test;\n\
         @state(version = 1) class Store { mut value: i32, }\n\
         pub fn run(value: i32) -> i32 { return value + 2; }"
        .into();
    assert_eq!(engine.reload_changed().expect("change scan reload"), 1);
    assert_eq!(engine.call::<Run>(&id, &1).expect("v2").value, 3);
    assert_eq!(
        engine
            .state_i32(&id, "store", "Store", "value")
            .expect("read state"),
        Some(9)
    );

    *script.write().expect("source lock") = "use host::test_host as test;\n\
         @state(version = 2) class Store { mut value: i32, mut extra: i32, }\n\
         pub fn run(value: i32) -> i32 { return value + 3; }"
        .into();
    assert!(matches!(engine.reload(&id), Err(EngineError::Reload(_, _))));
    assert_eq!(engine.status(&id), Some(PackageStatus::Enabled));
    assert_eq!(engine.call::<Run>(&id, &1).expect("rollback old").value, 3);

    *script.write().expect("source lock") = "use host::test_host as test;\n\
         @state(version = 1) class Store { mut value: i32, }\n\
         pub fn run(value: i32) -> i32 { return value + 4; }\n\
         @activation pub fn activate() -> i32 { let zero: i32 = 0; return 1 / zero; }"
        .into();
    assert!(matches!(
        engine.reload(&id),
        Err(EngineError::Activation(_, _))
    ));
    assert_eq!(engine.status(&id), Some(PackageStatus::Faulted));
    assert_eq!(engine.health().enabled_packages, 0);
}

#[test]
fn stress_reload_keeps_last_known_good_and_recovers_activation_faults() {
    let script = Arc::new(RwLock::new(
        "use host::test_host as test;\n\
         @state(version = 1) class Store { mut value: i32, }\n\
         pub fn run(value: i32) -> i32 { return value; }"
            .to_owned(),
    ));
    let source = SharedSource {
        id: SourceId::new("stress").expect("source ID"),
        policy: policy([ActivationPolicy::DefaultEnabled]),
        manifest: manifest("tests.stress", "default-enabled", ""),
        script: script.clone(),
    };
    let mut engine = builder(source).build().expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let id = PackageId::new("tests.stress").expect("package ID");
    engine
        .set_state_i32(&id, "store", "Store", 1, "value", 7)
        .expect("seed state");

    for generation in 1..=100 {
        *script.write().expect("source lock") = format!(
            "use host::test_host as test;\n\
             @state(version = 1) class Store {{ mut value: i32, }}\n\
             pub fn run(value: i32) -> i32 {{ return value + {generation}; }}"
        );
        engine.reload(&id).expect("successful reload");
        engine.tick().expect("maintenance tick");
        assert_eq!(
            engine
                .state_i32(&id, "store", "Store", "value")
                .expect("state"),
            Some(7)
        );
    }
    assert_eq!(engine.call::<Run>(&id, &1).expect("latest").value, 101);

    for _ in 0..100 {
        *script.write().expect("source lock") = "pub fn run(".into();
        assert!(matches!(
            engine.reload(&id),
            Err(EngineError::Source { .. } | EngineError::Diagnostic(_))
        ));
        assert_eq!(engine.call::<Run>(&id, &1).expect("syntax LKG").value, 101);
    }
    for _ in 0..100 {
        *script.write().expect("source lock") =
            "use host::test_host as test;\npub fn run(value: i32) -> i32 { return missing; }"
                .into();
        assert!(matches!(
            engine.reload(&id),
            Err(EngineError::Diagnostic(_))
        ));
        assert_eq!(engine.call::<Run>(&id, &1).expect("type LKG").value, 101);
    }
    for _ in 0..100 {
        *script.write().expect("source lock") = "use host::test_host as test;\n\
             @state(version = 2) class Store { mut value: i32, mut extra: i32, }\n\
             pub fn run(value: i32) -> i32 { return value + 999; }"
            .into();
        assert!(matches!(engine.reload(&id), Err(EngineError::Reload(_, _))));
        assert_eq!(
            engine.call::<Run>(&id, &1).expect("migration LKG").value,
            101
        );
    }

    for generation in 1..=10 {
        *script.write().expect("source lock") = "use host::test_host as test;\n\
             @state(version = 1) class Store { mut value: i32, }\n\
             pub fn run(value: i32) -> i32 { return value; }\n\
             @activation pub fn activate() -> i32 { let zero: i32 = 0; return 1 / zero; }"
            .into();
        assert!(matches!(
            engine.reload(&id),
            Err(EngineError::Activation(_, _))
        ));
        assert_eq!(engine.status(&id), Some(PackageStatus::Faulted));

        *script.write().expect("source lock") = format!(
            "use host::test_host as test;\n\
             @state(version = 1) class Store {{ mut value: i32, }}\n\
             pub fn run(value: i32) -> i32 {{ return value + {generation}; }}"
        );
        engine.reload(&id).expect("fault recovery reload");
        assert_eq!(engine.status(&id), Some(PackageStatus::Enabled));
    }

    engine.tick().expect("final maintenance");
    let health = engine.health();
    assert_eq!(health.tasks, 0);
    assert_eq!(health.requests, 0);
    assert_eq!(health.tokens, 0);
    assert_eq!(health.snapshots, 0);
    assert_eq!(health.queued_releases, 0);
    assert_eq!(engine.diagnostics().len(), 64);
    assert!(engine.inspection().dropped_diagnostics >= 236);
    assert!(
        engine
            .inspection()
            .packages
            .iter()
            .all(|package| package.recent_metrics.len() <= 32)
    );
    engine.shutdown().expect("clean shutdown");
}

#[test]
#[allow(clippy::too_many_lines)]
fn dev_engine_stabilizes_saves_and_commits_only_from_tick() {
    let script = Arc::new(RwLock::new(
        "use host::test_host as test;\n\
         pub fn run(value: i32) -> i32 { return value + 1; }"
            .to_owned(),
    ));
    let source = SharedSource {
        id: SourceId::new("dev").expect("source ID"),
        policy: policy([ActivationPolicy::DefaultEnabled]),
        manifest: manifest("tests.dev", "default-enabled", ""),
        script: script.clone(),
    };
    let mut engine = builder(source)
        .development(DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 2,
            ..DevelopmentConfig::default()
        })
        .build()
        .expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let id = PackageId::new("tests.dev").expect("package ID");

    *script.write().expect("source lock") =
        "use host::test_host as test;\npub fn run(value: i32) -> i32 { return value + 2; }".into();
    let observed = engine.tick().expect("observe save");
    assert!(
        observed
            .development_events
            .iter()
            .any(|event| matches!(event, DevelopmentEvent::ChangeDetected(_)))
    );
    assert_eq!(engine.call::<Run>(&id, &1).expect("old active").value, 2);

    let queued = engine.tick().expect("stabilize save");
    assert!(
        queued
            .development_events
            .iter()
            .any(|event| matches!(event, DevelopmentEvent::CompileQueued(_)))
    );
    assert_eq!(
        engine
            .call::<Run>(&id, &1)
            .expect("worker cannot commit")
            .value,
        2
    );

    let mut committed = None;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        let report = engine.tick().expect("candidate tick");
        committed = committed.or_else(|| {
            report
                .reloads
                .into_iter()
                .find(nexa_embed::ReloadReport::committed)
        });
        if committed.is_some() {
            break;
        }
    }
    let committed = committed.expect("committed Reload report");
    assert!(committed.compile_duration > std::time::Duration::ZERO);
    assert!(committed.verify_duration > std::time::Duration::ZERO);
    assert_eq!(
        committed.reload_duration,
        committed
            .quiesce_duration
            .saturating_add(committed.migration_duration)
            .saturating_add(committed.commit_duration)
            .saturating_add(committed.activation_duration)
    );
    assert_eq!(
        committed.total_change_to_visible_duration,
        committed
            .change_to_stable_duration
            .saturating_add(committed.queue_duration)
            .saturating_add(committed.compile_duration)
            .saturating_add(committed.verify_duration)
            .saturating_add(committed.ready_to_commit_duration)
            .saturating_add(committed.reload_duration)
    );
    assert_eq!(engine.call::<Run>(&id, &1).expect("new active").value, 3);

    *script.write().expect("source lock") =
        "use host::test_host as test;\npub fn run(value: i32) -> i32 { return missing; }".into();
    engine.tick().expect("observe invalid");
    engine.tick().expect("queue invalid");
    let mut failed = false;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        let report = engine.tick().expect("failure tick");
        failed |= report
            .development_events
            .iter()
            .any(|event| matches!(event, DevelopmentEvent::CompileFailed(_)));
        if failed {
            break;
        }
    }
    assert!(failed);
    assert_eq!(
        engine
            .call::<Run>(&id, &1)
            .expect("Last Known Good remains active")
            .value,
        3
    );
    engine.shutdown().expect("worker joins");
}

#[test]
fn source_removal_cancels_generation_keeps_active_and_recovers_after_reappearance() {
    let script = Arc::new(RwLock::new(
        "use host::test_host as test;\n\
         pub fn run(value: i32) -> i32 { return value + 1; }"
            .to_owned(),
    ));
    let available = Arc::new(RwLock::new(true));
    let source = RemovableSource {
        source: SharedSource {
            id: SourceId::new("removable").expect("source ID"),
            policy: policy([ActivationPolicy::DefaultEnabled]),
            manifest: manifest("tests.removable", "default-enabled", ""),
            script: Arc::clone(&script),
        },
        available: Arc::clone(&available),
    };
    let mut engine = builder(source)
        .development(DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        })
        .build()
        .expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let package_id = PackageId::new("tests.removable").expect("package ID");

    *script.write().expect("source lock") =
        "use host::test_host as test;\npub fn run(value: i32) -> i32 { return value + 2; }".into();
    engine.tick().expect("queue changed source");
    *available.write().expect("availability lock") = false;
    let missing = engine.tick().expect("observe source removal");
    assert!(
        missing
            .development_events
            .iter()
            .any(|event| matches!(event, DevelopmentEvent::SourceMissing(_)))
    );
    assert!(
        missing
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.diagnostic.code == nexa::ErrorCode::NX7001)
    );
    assert_eq!(engine.status(&package_id), Some(PackageStatus::Enabled));
    assert_eq!(
        engine
            .call::<Run>(&package_id, &1)
            .expect("old active Package remains callable")
            .value,
        2
    );
    for _ in 0..100 {
        engine.tick().expect("drain cancelled generation");
        if engine.inspection().development.generations_without_terminal == 0 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(
        engine.inspection().development.generations_without_terminal,
        0
    );

    *available.write().expect("availability lock") = true;
    let mut committed = false;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        committed |= engine
            .tick()
            .expect("reappearance tick")
            .reloads
            .iter()
            .any(nexa_embed::ReloadReport::committed);
        if committed {
            break;
        }
    }
    assert!(committed);
    assert_eq!(
        engine
            .call::<Run>(&package_id, &1)
            .expect("reappeared Candidate")
            .value,
        3
    );
    engine.shutdown().expect("shutdown");
}

#[test]
fn engine_disable_cancels_in_flight_candidate_without_late_ready_state() {
    let script = Arc::new(RwLock::new(
        "use host::test_host as test;\n\
         pub fn run(value: i32) -> i32 { return value + 1; }"
            .to_owned(),
    ));
    let source = SharedSource {
        id: SourceId::new("disable-race").expect("source ID"),
        policy: policy([ActivationPolicy::UserControlled]),
        manifest: manifest("tests.disable-race", "user-controlled", ""),
        script: Arc::clone(&script),
    };
    let mut engine = builder(source)
        .development(DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 1,
            ..DevelopmentConfig::default()
        })
        .build()
        .expect("build");
    engine.discover().expect("discover");
    let package_id = PackageId::new("tests.disable-race").expect("package ID");
    engine.enable(&package_id).expect("enable");
    let mut slow = String::from("use host::test_host as test;\npub fn run(value: i32) -> i32 {\n");
    for index in 0..1_000 {
        writeln!(&mut slow, "let value_{index}: i32 = {index};").expect("write slow source");
    }
    slow.push_str("return value + 2;\n}\n");
    *script.write().expect("source lock") = slow;
    engine.tick().expect("queue slow Candidate");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine
        .inspection()
        .development
        .worker
        .in_flight_package
        .as_ref()
        != Some(&package_id)
        && std::time::Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    assert_eq!(
        engine
            .inspection()
            .development
            .worker
            .in_flight_package
            .as_ref(),
        Some(&package_id)
    );
    engine
        .disable(&package_id)
        .expect("disable in-flight Package");
    let mut saw_ready = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        let report = engine.tick().expect("drain late Worker result");
        saw_ready |= report
            .development_events
            .iter()
            .any(|event| matches!(event, DevelopmentEvent::CandidateReady(_)));
        if engine.inspection().development.generations_without_terminal == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(!saw_ready);
    assert_eq!(engine.status(&package_id), Some(PackageStatus::Disabled));
    assert_eq!(
        engine.inspection().development.generations_without_terminal,
        0
    );
    engine.shutdown().expect("shutdown");
}

#[derive(Clone, Copy, Debug)]
struct GenerationAccountingEvidence {
    created: u64,
    terminal: u64,
    duplicate: u64,
    missing: u64,
}

fn accounting_script(_module: &str, delta: i32) -> String {
    format!(
        "use host::test_host as test;\n\
         pub fn run(value: i32) -> i32 {{ return value + {delta}; }}"
    )
}

fn accounting_engine(
    label: &str,
    activation: ActivationPolicy,
) -> (NexaEngine, Arc<RwLock<String>>, PackageId, String) {
    let package_name = format!("tests.accounting{label}");
    let initial = accounting_script(&package_name, 1);
    let script = Arc::new(RwLock::new(initial.clone()));
    let source = SharedSource {
        id: SourceId::new(format!("accounting-{label}")).expect("source ID"),
        policy: policy([activation]),
        manifest: manifest(
            &package_name,
            match activation {
                ActivationPolicy::DefaultEnabled => "default-enabled",
                ActivationPolicy::UserControlled => "user-controlled",
                ActivationPolicy::Required => "required",
                ActivationPolicy::Programmatic => "programmatic",
            },
            "",
        ),
        script: Arc::clone(&script),
    };
    let mut engine = builder(source)
        .development(DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 3,
            ..DevelopmentConfig::default()
        })
        .build()
        .expect("build accounting Engine");
    engine.discover().expect("discover accounting Package");
    let package_id = PackageId::new(package_name).expect("Package ID");
    if activation == ActivationPolicy::DefaultEnabled {
        engine.enable_defaults().expect("enable default Package");
    } else {
        engine.enable(&package_id).expect("enable Package");
    }
    (engine, script, package_id, initial)
}

fn assert_generation_accounting(
    engine: &NexaEngine,
    package_id: &PackageId,
    created: u64,
    terminal: u64,
    expected_latest: CandidateTerminalKind,
) -> GenerationAccountingEvidence {
    let inspection = engine.inspection();
    let package = inspection
        .packages
        .iter()
        .find(|package| package.package_id == *package_id)
        .expect("accounting Package inspection");
    assert_eq!(package.candidate_generation, created);
    assert_eq!(package.terminal_generations, terminal);
    assert_eq!(package.duplicate_terminals, 0);
    assert_eq!(
        package.generations_without_terminal,
        created.saturating_sub(terminal)
    );
    assert_eq!(package.latest_terminal_generation, Some(terminal));
    assert_eq!(package.latest_terminal_kind, Some(expected_latest));
    assert_eq!(inspection.development.created_generations, created);
    assert_eq!(inspection.development.terminal_generations, terminal);
    assert_eq!(inspection.development.duplicate_terminals, 0);
    assert_eq!(
        inspection.development.generations_without_terminal,
        created.saturating_sub(terminal)
    );
    GenerationAccountingEvidence {
        created,
        terminal,
        duplicate: inspection.development.duplicate_terminals,
        missing: inspection.development.generations_without_terminal,
    }
}

fn wait_for_accounting_balance(engine: &mut NexaEngine, package_id: &PackageId, created: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let inspection = engine.inspection();
        if inspection.development.terminal_generations == created
            && inspection.development.generations_without_terminal == 0
        {
            return;
        }
        engine.tick().expect("drain accounting Candidate");
        std::thread::yield_now();
    }
    panic!(
        "Generation accounting for {package_id} did not balance: {:?}",
        engine.inspection().development
    );
}

fn run_prequeue_hash_replacement() -> GenerationAccountingEvidence {
    let (mut engine, script, package_id, _) =
        accounting_engine("replace", ActivationPolicy::DefaultEnabled);
    *script.write().expect("source lock") = accounting_script(package_id.as_str(), 2);
    engine.tick().expect("observe B");
    *script.write().expect("source lock") = accounting_script(package_id.as_str(), 3);
    let replaced = engine.tick().expect("observe C");
    assert!(replaced.development_events.iter().any(|event| {
        matches!(event, DevelopmentEvent::CandidateSuperseded(data)
            if data.identity.generation == 1)
    }));
    assert_generation_accounting(
        &engine,
        &package_id,
        2,
        1,
        CandidateTerminalKind::SupersededBeforeCompile,
    );
    wait_for_accounting_balance(&mut engine, &package_id, 2);
    assert_eq!(
        engine
            .call::<Run>(&package_id, &1)
            .expect("C becomes active")
            .value,
        4
    );
    engine.shutdown().expect("shutdown replacement Engine");
    assert_generation_accounting(&engine, &package_id, 2, 2, CandidateTerminalKind::Compiled)
}

fn run_prequeue_revert_to_active() -> GenerationAccountingEvidence {
    let (mut engine, script, package_id, initial) =
        accounting_engine("revert", ActivationPolicy::DefaultEnabled);
    *script.write().expect("source lock") = accounting_script(package_id.as_str(), 2);
    engine.tick().expect("observe B");
    *script.write().expect("source lock") = initial;
    let reverted = engine.tick().expect("revert to A");
    assert!(reverted.development_events.iter().any(|event| {
        matches!(event, DevelopmentEvent::CandidateSuperseded(data)
            if data.identity.generation == 1)
    }));
    assert_eq!(
        engine
            .call::<Run>(&package_id, &1)
            .expect("A remains active")
            .value,
        2
    );
    engine.shutdown().expect("shutdown revert Engine");
    assert_generation_accounting(
        &engine,
        &package_id,
        1,
        1,
        CandidateTerminalKind::SupersededBeforeCompile,
    )
}

fn run_prequeue_source_removal() -> GenerationAccountingEvidence {
    let package_id = PackageId::new("tests.accountingremoval").expect("Package ID");
    let initial = accounting_script(package_id.as_str(), 1);
    let script = Arc::new(RwLock::new(initial));
    let available = Arc::new(RwLock::new(true));
    let source = RemovableSource {
        source: SharedSource {
            id: SourceId::new("accounting-removal").expect("source ID"),
            policy: policy([ActivationPolicy::DefaultEnabled]),
            manifest: manifest(package_id.as_str(), "default-enabled", ""),
            script: Arc::clone(&script),
        },
        available: Arc::clone(&available),
    };
    let mut engine = builder(source)
        .development(DevelopmentConfig {
            scan_interval_ticks: 1,
            stable_scan_count: 3,
            ..DevelopmentConfig::default()
        })
        .build()
        .expect("build removal Engine");
    engine.discover().expect("discover removal Package");
    engine.enable_defaults().expect("enable removal Package");
    *script.write().expect("source lock") = accounting_script(package_id.as_str(), 2);
    engine.tick().expect("observe B");
    *available.write().expect("availability lock") = false;
    let removed = engine.tick().expect("remove source");
    assert!(removed.development_events.iter().any(|event| {
        matches!(event, DevelopmentEvent::CandidateCancelled(data)
            if data.identity.generation == 1)
    }));
    engine.shutdown().expect("shutdown removal Engine");
    assert_generation_accounting(
        &engine,
        &package_id,
        1,
        1,
        CandidateTerminalKind::CancelledBySourceRemoval,
    )
}

fn run_prequeue_disable() -> GenerationAccountingEvidence {
    let (mut engine, script, package_id, _) =
        accounting_engine("disable", ActivationPolicy::UserControlled);
    *script.write().expect("source lock") = accounting_script(package_id.as_str(), 2);
    engine.tick().expect("observe B");
    engine.disable(&package_id).expect("disable before stable");
    assert_eq!(engine.status(&package_id), Some(PackageStatus::Disabled));
    let evidence = assert_generation_accounting(
        &engine,
        &package_id,
        1,
        1,
        CandidateTerminalKind::CancelledByDisable,
    );
    engine.shutdown().expect("shutdown disabled Engine");
    evidence
}

fn run_prequeue_shutdown() -> GenerationAccountingEvidence {
    let (mut engine, script, package_id, _) =
        accounting_engine("shutdown", ActivationPolicy::DefaultEnabled);
    *script.write().expect("source lock") = accounting_script(package_id.as_str(), 2);
    engine.tick().expect("observe B");
    engine.shutdown().expect("shutdown before stable");
    assert!(!engine.inspection().development.worker_running);
    assert_generation_accounting(
        &engine,
        &package_id,
        1,
        1,
        CandidateTerminalKind::CancelledByShutdown,
    )
}

#[test]
fn prequeue_hash_replacement_supersedes_previous_generation() {
    let evidence = run_prequeue_hash_replacement();
    assert_eq!(evidence.missing, 0);
}

#[test]
fn prequeue_revert_to_active_supersedes_observed_generation() {
    let evidence = run_prequeue_revert_to_active();
    assert_eq!(evidence.missing, 0);
}

#[test]
fn prequeue_source_removal_cancels_observed_generation() {
    let evidence = run_prequeue_source_removal();
    assert_eq!(evidence.missing, 0);
}

#[test]
fn prequeue_disable_cancels_observed_generation() {
    let evidence = run_prequeue_disable();
    assert_eq!(evidence.missing, 0);
}

#[test]
fn prequeue_shutdown_cancels_observed_generation() {
    let evidence = run_prequeue_shutdown();
    assert_eq!(evidence.missing, 0);
}

#[test]
fn generation_accounting_machine_report_uses_real_engine_inspection() {
    let evidence = [
        run_prequeue_hash_replacement(),
        run_prequeue_revert_to_active(),
        run_prequeue_source_removal(),
        run_prequeue_disable(),
        run_prequeue_shutdown(),
    ];
    let created = evidence.iter().map(|item| item.created).sum::<u64>();
    let terminal = evidence.iter().map(|item| item.terminal).sum::<u64>();
    let duplicate = evidence.iter().map(|item| item.duplicate).sum::<u64>();
    let missing = evidence.iter().map(|item| item.missing).sum::<u64>();
    assert_eq!(created, terminal);
    assert_eq!(duplicate, 0);
    assert_eq!(missing, 0);
    let report = format!(
        "{{\n  \"schema\": 1,\n  \"scenarioCount\": 5,\n  \
         \"createdGenerations\": {created},\n  \"terminalGenerations\": {terminal},\n  \
         \"duplicateTerminals\": {duplicate},\n  \
         \"generationsWithoutTerminal\": {missing},\n  \
         \"supersededBeforeCompile\": 2,\n  \
         \"cancelledBySourceRemoval\": 1,\n  \
         \"cancelledByDisable\": 1,\n  \
         \"cancelledByShutdown\": 1,\n  \"status\": \"PASS\"\n}}\n"
    );
    if let Some(path) = std::env::var_os("NEXA_GENERATION_ACCOUNTING_REPORT") {
        let path = PathBuf::from(path);
        std::fs::create_dir_all(path.parent().expect("report parent"))
            .expect("create report directory");
        std::fs::write(path, &report).expect("write Generation accounting report");
    }
    println!("{report}");
}

#[test]
fn directory_source_rejects_traversal_and_persistence_restores_selection() {
    let root = unique_temp("directory");
    let package_root = root.join("package");
    std::fs::create_dir_all(&package_root).expect("create package");
    std::fs::write(
        package_root.join("package.toml"),
        manifest("tests.path", "user-controlled", "")
            .replace("entry = \"tests.path\"", "entry = \"../outside\""),
    )
    .expect("write manifest");
    std::fs::write(
        root.join("outside.nexa"),
        "pub fn run(value: i32) -> i32 { return value; }",
    )
    .expect("write outside script");
    let directory = DirectorySource::new(
        SourceId::new("directory").expect("source ID"),
        &root,
        policy([ActivationPolicy::UserControlled]),
    );
    assert!(
        directory
            .discover(&CandidateBuildContext::new(IDL_SOURCE.as_bytes().to_vec()))
            .is_err()
    );

    let storage = unique_temp("persistence");
    let make = || {
        builder(source(
            "persisted",
            "tests.persisted",
            "user-controlled",
            "pub fn run(value: i32) -> i32 { return value; }",
        ))
        .storage_dir(&storage)
        .build()
        .expect("build persisted engine")
    };
    let id = PackageId::new("tests.persisted").expect("package ID");
    {
        let mut engine = make();
        engine.discover().expect("discover");
        engine.enable(&id).expect("enable");
        engine.shutdown().expect("shutdown");
    }
    let mut restored = make();
    restored.discover().expect("discover restored");
    restored.enable_defaults().expect("restore enabled choice");
    assert_eq!(restored.status(&id), Some(PackageStatus::Enabled));
}

fn unique_temp(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "nexa-m3-{label}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    if Path::new(&path).exists() {
        std::fs::remove_dir_all(&path).expect("remove stale test directory");
    }
    path
}
