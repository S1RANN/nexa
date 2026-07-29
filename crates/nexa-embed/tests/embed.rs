use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilitySet, DevelopmentConfig, DevelopmentEvent,
    DirectorySource, EngineError, EntitlementId, EntitlementResolver, HostContract, MemorySource,
    NexaEngine, PackageCandidate, PackageId, PackagePolicy, PackageRuntimeLimits, PackageSource,
    PackageSourceError, PackageStatus, SourceId, TrustLevel,
};
use nexa_runtime::{
    HostCallOutcome, HostRegistry, HostTrap, ResourceContext, RuntimeHostArgs, RuntimeValue,
    ScriptArgumentRequirements, ScriptCallError, ScriptCallWriter, ScriptExport,
    ScriptOutputReader, Signature, StableId, ValueType,
};

const IDL_SOURCE: &str = "interface TestHost {
    enum WaitError { Cancelled }
    request(return_error, trap) fn wait(value: i32) -> request<Result<i32, WaitError>>;
    export Run(value: i32) -> i32;
}";
const RUN_ID: StableId = StableId(0xf1c5_6273_0ddd_ab52);

struct Registry(StableId);

impl HostRegistry for Registry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id == 0 {
            let pending = context
                .create_request()
                .map_err(|_| HostTrap::ResourceCapacity)?;
            Ok(HostCallOutcome::Pending(pending.request))
        } else {
            Err(HostTrap::UnknownFunction(id))
        }
    }
}

struct TrappingRegistry(StableId);

impl HostRegistry for TrappingRegistry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call_runtime(
        &mut self,
        _: u32,
        _: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::Panicked)
    }
}

struct MeteredRegistry(StableId);

impl HostRegistry for MeteredRegistry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 {
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

struct MeterRun;

impl ScriptExport for MeterRun {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = StableId(0x45a3_6e76_9a57_b233);
    const NAME: &'static str = "Run";

    fn signature() -> Signature {
        Run::signature()
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
    let idl = nexa_idl::parse(IDL_SOURCE).expect("test IDL");
    HostContract {
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
         name = \"Test\"\n\
         version = \"1.0.0\"\n\
         entry = \"main.nexa\"\n\
         activation = \"{activation}\"\n\
         handler_fuel = 20000\n\
         capabilities = []\n\
         entitlement = \"{entitlement}\"\n"
    )
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
    .package(
        manifest(package, activation, ""),
        format!("module tests.fixture;\nimport test;\n{script}"),
    )
}

fn builder(source: impl PackageSource + 'static) -> nexa_embed::NexaEngineBuilder {
    let contract = contract();
    let hash = contract.interface_hash;
    NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(Registry(hash)) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<Run>()
}

#[test]
fn memory_source_enables_calls_disables_and_shuts_down() {
    let mut engine = builder(source(
        "memory",
        "tests.basic",
        "default-enabled",
        "fn Run(value: i32) -> i32 { return value + 1; }",
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
    const METER_IDL: &str = "interface MeterHost {
        sync fuel 11 fn expensive(value: i32) -> i32;
        export Run(value: i32) -> i32;
    }";
    let idl = nexa_idl::parse(METER_IDL).expect("meter IDL");
    let contract = HostContract {
        interface_name: "MeterHost",
        canonical_idl: METER_IDL,
        interface_hash: nexa_idl::exact_hash(&idl),
        generator_schema_version: nexa_runtime::HOST_CONTRACT_SCHEMA_VERSION,
    };
    let hash = contract.interface_hash;
    let source = MemorySource::new(
        SourceId::new("meter").expect("source ID"),
        policy([ActivationPolicy::DefaultEnabled]),
    )
    .package(
        manifest("tests.meter", "default-enabled", ""),
        "module tests.meter;
         import meter;
         fn Run(value: i32) -> i32 { return meter.expensive(value); }",
    );
    let mut engine = NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(MeteredRegistry(hash)) as Box<dyn HostRegistry>
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
        "fn Run(value: i32) -> i32 { return value; }",
    );
    let right = source(
        "same",
        "tests.right",
        "user-controlled",
        "fn Run(value: i32) -> i32 { return value; }",
    );
    assert!(matches!(
        builder(left).package_source(right).build(),
        Err(EngineError::DuplicateSourceId(_))
    ));

    let mut engine = builder(source(
        "left",
        "tests.duplicate",
        "user-controlled",
        "fn Run(value: i32) -> i32 { return value; }",
    ))
    .package_source(source(
        "right",
        "tests.duplicate",
        "user-controlled",
        "fn Run(value: i32) -> i32 { return value + 1; }",
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
        manifest("tests.licensed", "user-controlled", "tests.license"),
        "module tests.licensed;\n\
         import test;\n\
         fn Run(value: i32) -> i32 { return value; }",
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
        "fn Run(value: i32) -> i32 { return value; }",
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

    fn discover(&self) -> Result<Vec<PackageCandidate>, PackageSourceError> {
        let manifest = nexa_embed::PackageManifest::parse(&self.manifest, &self.policy)?;
        Ok(vec![PackageCandidate::new(
            manifest,
            self.manifest.clone(),
            self.script.read().expect("source lock").clone(),
        )])
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

    fn discover(&self) -> Result<Vec<PackageCandidate>, PackageSourceError> {
        if !*self.available.read().expect("availability lock") {
            return Err(PackageSourceError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Package source disappeared",
            )));
        }
        self.source.discover()
    }
}

#[test]
fn reload_uses_fresh_source_and_rolls_back_compile_failure() {
    let script = Arc::new(RwLock::new(
        "module tests.reload;\nimport test;\n\
         fn Run(value: i32) -> i32 { return value + 1; }"
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
    *script.write().expect("source lock") = "module tests.reload;\nimport test;\n\
         fn Run(value: i32) -> i32 { return value + 2; }"
        .into();
    engine.reload(&id).expect("reload v2");
    assert_eq!(engine.call::<Run>(&id, &1).expect("call v2").value, 3);
    *script.write().expect("source lock") = "fn Run(".into();
    assert!(matches!(
        engine.reload(&id),
        Err(EngineError::Diagnostic(_))
    ));
    assert_eq!(engine.status(&id), Some(PackageStatus::Enabled));
    assert_eq!(
        engine
            .call::<Run>(&id, &1)
            .expect("old module retained")
            .value,
        3
    );
}

#[test]
fn handler_yield_fault_isolated_and_dispatch_order_is_stable() {
    let mut engine = builder(source(
        "normal",
        "tests.a-normal",
        "default-enabled",
        "fn Run(value: i32) -> i32 { return value; }",
    ))
    .package_source(source(
        "yielding",
        "tests.z-yield",
        "default-enabled",
        "task fn Run(value: i32) -> i32 { yield; return value; }",
    ))
    .build()
    .expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let results = engine.dispatch::<Run>(&7);
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
    for (source_id, package_id, manifest_patch, script) in [
        (
            "trap",
            "tests.trap",
            "",
            "fn Run(value: i32) -> i32 { let zero: i32 = 0; return value / zero; }",
        ),
        (
            "fuel",
            "tests.fuel",
            "handler_fuel = 1",
            "fn Run(value: i32) -> i32 { return value + 1; }",
        ),
        (
            "wait",
            "tests.wait",
            "",
            "task fn Run(value: i32) -> i32 {
                 let result: Result<i32, WaitError> = await test.wait(value);
                 return match result { Ok(found) => found, Err(error) => 0 };
             }",
        ),
    ] {
        let raw_manifest = manifest(package_id, "default-enabled", "")
            .replace("handler_fuel = 20000", manifest_patch);
        let source = MemorySource::new(
            SourceId::new(source_id).expect("source ID"),
            policy([ActivationPolicy::DefaultEnabled]),
        )
        .package(
            raw_manifest,
            format!("module tests.{source_id};\nimport test;\n{script}"),
        );
        let mut engine = builder(source).build().expect("build");
        engine.discover().expect("discover");
        engine.enable_defaults().expect("enable");
        let id = PackageId::new(package_id).expect("package ID");
        assert!(matches!(
            engine.call::<Run>(&id, &7),
            Err(EngineError::Handler(_, _))
        ));
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
        "task fn Run(value: i32) -> i32 {
             let result: Result<i32, WaitError> = await test.wait(value);
             return match result { Ok(found) => found, Err(error) => 0 };
         }",
    );
    let contract = contract();
    let hash = contract.interface_hash;
    let mut engine = NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(TrappingRegistry(hash)) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<Run>()
        .build()
        .expect("build");
    engine.discover().expect("discover");
    engine.enable_defaults().expect("enable");
    let id = PackageId::new("tests.host-trap").expect("package ID");
    assert!(matches!(
        engine.call::<Run>(&id, &7),
        Err(EngineError::Handler(_, _))
    ));
    let diagnostic = engine
        .diagnostics()
        .into_iter()
        .find(|diagnostic| diagnostic.package_id.as_ref() == Some(&id))
        .expect("runtime diagnostic");
    assert!(diagnostic.file.is_some());
    assert_eq!(diagnostic.context.export.as_deref(), Some("Run"));
    assert!(
        diagnostic
            .related
            .iter()
            .any(|related| related.message.contains("at Run"))
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
        nexa_embed::PackageManifest::parse(&excessive, &policy),
        Err(nexa_embed::ManifestError::CapabilityCeiling)
    ));
    let illegal_activation = manifest("tests.policy", "required", "");
    assert!(matches!(
        nexa_embed::PackageManifest::parse(&illegal_activation, &policy),
        Err(nexa_embed::ManifestError::ActivationNotAllowed)
    ));

    let mut engine = builder(source(
        "broken",
        "tests.broken",
        "default-enabled",
        "fn Run(",
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
        "module tests.state;\nimport test;\n\
         @stateful(1) class Store { value: i32; }\n\
         fn Run(value: i32) -> i32 { return value + 1; }"
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

    *script.write().expect("source lock") = "module tests.state;\nimport test;\n\
         @stateful(1) class Store { value: i32; }\n\
         fn Run(value: i32) -> i32 { return value + 2; }"
        .into();
    assert_eq!(engine.reload_changed().expect("change scan reload"), 1);
    assert_eq!(engine.call::<Run>(&id, &1).expect("v2").value, 3);
    assert_eq!(
        engine
            .state_i32(&id, "store", "Store", "value")
            .expect("read state"),
        Some(9)
    );

    *script.write().expect("source lock") = "module tests.state;\nimport test;\n\
         @stateful(2) class Store { value: i32; extra: i32; }\n\
         fn Run(value: i32) -> i32 { return value + 3; }"
        .into();
    assert!(matches!(engine.reload(&id), Err(EngineError::Reload(_, _))));
    assert_eq!(engine.status(&id), Some(PackageStatus::Enabled));
    assert_eq!(engine.call::<Run>(&id, &1).expect("rollback old").value, 3);

    *script.write().expect("source lock") = "module tests.state;\nimport test;\n\
         @stateful(1) class Store { value: i32; }\n\
         fn Run(value: i32) -> i32 { return value + 4; }\n\
         @activation fn activate(value: i32) -> i32 { return value; }"
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
        "module tests.stress;\nimport test;\n\
         @stateful(1) class Store { value: i32; }\n\
         fn Run(value: i32) -> i32 { return value; }"
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
            "module tests.stress;\nimport test;\n\
             @stateful(1) class Store {{ value: i32; }}\n\
             fn Run(value: i32) -> i32 {{ return value + {generation}; }}"
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
        *script.write().expect("source lock") = "fn Run(".into();
        assert!(matches!(
            engine.reload(&id),
            Err(EngineError::Diagnostic(_))
        ));
        assert_eq!(engine.call::<Run>(&id, &1).expect("syntax LKG").value, 101);
    }
    for _ in 0..100 {
        *script.write().expect("source lock") =
            "module tests.stress;\nimport test;\nfn Run(value: i32) -> i32 { return missing; }"
                .into();
        assert!(matches!(
            engine.reload(&id),
            Err(EngineError::Diagnostic(_))
        ));
        assert_eq!(engine.call::<Run>(&id, &1).expect("type LKG").value, 101);
    }
    for _ in 0..100 {
        *script.write().expect("source lock") = "module tests.stress;\nimport test;\n\
             @stateful(2) class Store { value: i32; extra: i32; }\n\
             fn Run(value: i32) -> i32 { return value + 999; }"
            .into();
        assert!(matches!(engine.reload(&id), Err(EngineError::Reload(_, _))));
        assert_eq!(
            engine.call::<Run>(&id, &1).expect("migration LKG").value,
            101
        );
    }

    for generation in 1..=10 {
        *script.write().expect("source lock") = "module tests.stress;\nimport test;\n\
             @stateful(1) class Store { value: i32; }\n\
             fn Run(value: i32) -> i32 { return value; }\n\
             @activation fn activate(value: i32) -> i32 { return value; }"
            .into();
        assert!(matches!(
            engine.reload(&id),
            Err(EngineError::Activation(_, _))
        ));
        assert_eq!(engine.status(&id), Some(PackageStatus::Faulted));

        *script.write().expect("source lock") = format!(
            "module tests.stress;\nimport test;\n\
             @stateful(1) class Store {{ value: i32; }}\n\
             fn Run(value: i32) -> i32 {{ return value + {generation}; }}"
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
        "module tests.dev;\nimport test;\n\
         fn Run(value: i32) -> i32 { return value + 1; }"
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
        "module tests.dev;\nimport test;\nfn Run(value: i32) -> i32 { return value + 2; }".into();
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
        "module tests.dev;\nimport test;\nfn Run(value: i32) -> i32 { return missing; }".into();
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
        "module tests.removable;\nimport test;\n\
         fn Run(value: i32) -> i32 { return value + 1; }"
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
        "module tests.removable;\nimport test;\nfn Run(value: i32) -> i32 { return value + 2; }"
            .into();
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
        "module tests.disable_race;\nimport test;\n\
         fn Run(value: i32) -> i32 { return value + 1; }"
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
    let mut slow =
        String::from("module tests.disable_race;\nimport test;\nfn Run(value: i32) -> i32 {\n");
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

#[test]
fn directory_source_rejects_traversal_and_persistence_restores_selection() {
    let root = unique_temp("directory");
    let package_root = root.join("package");
    std::fs::create_dir_all(&package_root).expect("create package");
    std::fs::write(
        package_root.join("package.toml"),
        manifest("tests.path", "user-controlled", "")
            .replace("entry = \"main.nexa\"", "entry = \"../outside.nexa\""),
    )
    .expect("write manifest");
    std::fs::write(
        root.join("outside.nexa"),
        "fn Run(value: i32) -> i32 { return value; }",
    )
    .expect("write outside script");
    let directory = DirectorySource::new(
        SourceId::new("directory").expect("source ID"),
        &root,
        policy([ActivationPolicy::UserControlled]),
    );
    assert!(directory.discover().is_err());

    let storage = unique_temp("persistence");
    let make = || {
        builder(source(
            "persisted",
            "tests.persisted",
            "user-controlled",
            "fn Run(value: i32) -> i32 { return value; }",
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
