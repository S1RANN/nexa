use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilitySet, DirectorySource, EmbedError, EntitlementId,
    EntitlementResolver, HostContract, MemorySource, NexaEmbed, PackageCandidate, PackageId,
    PackagePolicy, PackageRuntimeLimits, PackageSource, PackageSourceError, PackageStatus,
    SourceId, TrustLevel,
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

fn builder(source: impl PackageSource + 'static) -> nexa_embed::NexaEmbedBuilder {
    let contract = contract();
    let hash = contract.interface_hash;
    NexaEmbed::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(Registry(hash)) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<Run>()
}

#[test]
fn memory_source_enables_calls_disables_and_shuts_down() {
    let mut embed = builder(source(
        "memory",
        "tests.basic",
        "default-enabled",
        "fn Run(value: i32) -> i32 { return value + 1; }",
    ))
    .build()
    .expect("build");
    embed.discover().expect("discover");
    embed.enable_defaults().expect("enable");
    let id = PackageId::new("tests.basic").expect("package ID");
    assert_eq!(embed.call::<Run>(&id, &41).expect("call").value, 42);
    assert_eq!(embed.health().enabled_packages, 1);
    embed.disable(&id).expect("disable");
    assert_eq!(embed.health().enabled_packages, 0);
    assert_eq!(embed.health().host_pending_releases, 0);
    embed.shutdown().expect("shutdown");
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
        Err(EmbedError::DuplicateSourceId(_))
    ));

    let mut embed = builder(source(
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
    let packages = embed.discover().expect("discover duplicates");
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
    let mut embed = builder(licensed)
        .entitlements(resolver.clone())
        .build()
        .expect("build");
    embed.discover().expect("discover");
    let id = PackageId::new("tests.licensed").expect("package ID");
    assert_eq!(embed.status(&id), Some(PackageStatus::Locked));
    resolver
        .0
        .write()
        .expect("entitlement lock")
        .push(entitlement);
    embed.refresh_entitlements().expect("unlock");
    assert_eq!(embed.status(&id), Some(PackageStatus::Disabled));
    embed.enable(&id).expect("enable unlocked package");

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
        Err(EmbedError::RequiredPackage(_))
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
    let mut embed = builder(source).build().expect("build");
    embed.discover().expect("discover");
    embed.enable_defaults().expect("enable");
    let id = PackageId::new("tests.reload").expect("package ID");
    assert_eq!(embed.call::<Run>(&id, &1).expect("call v1").value, 2);
    *script.write().expect("source lock") = "module tests.reload;\nimport test;\n\
         fn Run(value: i32) -> i32 { return value + 2; }"
        .into();
    embed.reload(&id).expect("reload v2");
    assert_eq!(embed.call::<Run>(&id, &1).expect("call v2").value, 3);
    *script.write().expect("source lock") = "fn Run(".into();
    assert!(matches!(embed.reload(&id), Err(EmbedError::Reload(_, _))));
    assert_eq!(embed.status(&id), Some(PackageStatus::Enabled));
    assert_eq!(
        embed
            .call::<Run>(&id, &1)
            .expect("old module retained")
            .value,
        3
    );
}

#[test]
fn handler_yield_fault_isolated_and_dispatch_order_is_stable() {
    let mut embed = builder(source(
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
    embed.discover().expect("discover");
    embed.enable_defaults().expect("enable");
    let results = embed.dispatch::<Run>(&7);
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0]
            .as_ref()
            .expect("normal output")
            .package_id
            .as_str(),
        "tests.a-normal"
    );
    assert!(matches!(results[1], Err(EmbedError::Handler(_, _))));
    assert_eq!(
        embed.status(&PackageId::new("tests.a-normal").expect("package ID")),
        Some(PackageStatus::Enabled)
    );
    assert_eq!(
        embed.status(&PackageId::new("tests.z-yield").expect("package ID")),
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
        let mut embed = builder(source).build().expect("build");
        embed.discover().expect("discover");
        embed.enable_defaults().expect("enable");
        let id = PackageId::new(package_id).expect("package ID");
        assert!(matches!(
            embed.call::<Run>(&id, &7),
            Err(EmbedError::Handler(_, _))
        ));
        assert_eq!(embed.status(&id), Some(PackageStatus::Faulted));
        assert_eq!(embed.health().host_pending_releases, 0);
        assert!(
            embed
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.package_id.as_ref() == Some(&id))
        );
    }
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

    let mut embed = builder(source(
        "broken",
        "tests.broken",
        "default-enabled",
        "fn Run(",
    ))
    .build()
    .expect("build");
    embed.discover().expect("discover");
    let id = PackageId::new("tests.broken").expect("package ID");
    assert!(embed.enable(&id).is_err());
    assert_eq!(embed.status(&id), Some(PackageStatus::Faulted));
    assert_eq!(embed.health().enabled_packages, 0);
    assert_eq!(embed.health().host_pending_releases, 0);
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
    let mut embed = builder(source).build().expect("build");
    embed.discover().expect("discover");
    embed.enable_defaults().expect("enable");
    let id = PackageId::new("tests.state").expect("package ID");
    embed
        .set_state_i32(&id, "store", "Store", 1, "value", 9)
        .expect("insert state");

    *script.write().expect("source lock") = "module tests.state;\nimport test;\n\
         @stateful(1) class Store { value: i32; }\n\
         fn Run(value: i32) -> i32 { return value + 2; }"
        .into();
    assert_eq!(embed.reload_changed().expect("change scan reload"), 1);
    assert_eq!(embed.call::<Run>(&id, &1).expect("v2").value, 3);
    assert_eq!(
        embed
            .state_i32(&id, "store", "Store", "value")
            .expect("read state"),
        Some(9)
    );

    *script.write().expect("source lock") = "module tests.state;\nimport test;\n\
         @stateful(2) class Store { value: i32; extra: i32; }\n\
         fn Run(value: i32) -> i32 { return value + 3; }"
        .into();
    assert!(matches!(embed.reload(&id), Err(EmbedError::Reload(_, _))));
    assert_eq!(embed.status(&id), Some(PackageStatus::Enabled));
    assert_eq!(embed.call::<Run>(&id, &1).expect("rollback old").value, 3);

    *script.write().expect("source lock") = "module tests.state;\nimport test;\n\
         @stateful(1) class Store { value: i32; }\n\
         fn Run(value: i32) -> i32 { return value + 4; }\n\
         @activation fn activate(value: i32) -> i32 { return value; }"
        .into();
    assert!(matches!(embed.reload(&id), Err(EmbedError::Reload(_, _))));
    assert_eq!(embed.status(&id), Some(PackageStatus::Faulted));
    assert_eq!(embed.health().enabled_packages, 0);
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
        .expect("build persisted embed")
    };
    let id = PackageId::new("tests.persisted").expect("package ID");
    {
        let mut embed = make();
        embed.discover().expect("discover");
        embed.enable(&id).expect("enable");
        embed.shutdown().expect("shutdown");
    }
    let mut restored = make();
    restored.discover().expect("discover restored");
    restored.enable_defaults().expect("restore enabled choice");
    assert_eq!(restored.status(&id), Some(PackageStatus::Enabled));
}

fn unique_temp(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "nexa-embed-{label}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    if Path::new(&path).exists() {
        std::fs::remove_dir_all(&path).expect("remove stale test directory");
    }
    path
}
