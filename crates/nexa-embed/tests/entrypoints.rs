use std::collections::BTreeMap;
use std::sync::OnceLock;

use nexa::prelude::{
    FunctionEffect, HostCallOutcome, HostFunctionSlot, HostRegistry, HostTrap,
    ResolvedHostFunction, ResourceContext, RuntimeHostArgs, RuntimeValue,
    ScriptArgumentRequirements, ScriptArguments, ScriptCallError, ScriptCallWriter, ScriptExport,
    ScriptOutputReader, ScriptSignature, StableId, ValueType,
};
use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilitySet, EngineError, HostContract, MemoryPackage,
    MemorySource, NexaEngine, PackageId, PackagePolicy, PackageRuntimeLimits, PackageStatus,
    SourceId, TrustLevel,
};

const CONTRACT_SOURCE: &str = r"contract SnakeEntrypoints {
    host {}

    nexa {
        fn on_event(value: i32) -> i32;
        fn choose_food_spawn(value: i32) -> i32;
        fn calculate_food_effect(value: i32) -> i32;
    }
}";

macro_rules! i32_entrypoint {
    ($marker:ident, $name:literal, $stable_id:literal, $contract_slot:expr) => {
        struct $marker;

        impl ScriptExport for $marker {
            type Args = i32;
            type Output = i32;

            const STABLE_ID: StableId = StableId($stable_id);
            const NAME: &'static str = $name;
            const CONTRACT_SLOT: usize = $contract_slot;
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
    };
}

// NIDL v2 declaration identities for the validated `SnakeEntrypoints` Contract.
i32_entrypoint!(OnEvent, "on_event", 0xefff_24e2_9dbd_2cb4, 2);
i32_entrypoint!(
    ChooseFoodSpawn,
    "choose_food_spawn",
    0x0418_ea07_84bf_08cf,
    1
);
i32_entrypoint!(
    CalculateFoodEffect,
    "calculate_food_effect",
    0xe9ac_e9e4_2957_0132,
    0
);
i32_entrypoint!(
    NotDeclared,
    "not_declared",
    0x1111_2222_3333_4444,
    usize::MAX
);

struct EmptyRegistry(StableId);

impl HostRegistry for EmptyRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn resolve_function(&self, _: StableId) -> Option<ResolvedHostFunction<'_>> {
        None
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

fn contract() -> HostContract {
    static CONTRACT: OnceLock<HostContract> = OnceLock::new();
    *CONTRACT.get_or_init(|| {
        let parsed = nexa::parse_contract(CONTRACT_SOURCE).expect("Snake entrypoint Contract");
        for (name, expected) in [
            ("on_event", OnEvent::STABLE_ID),
            ("choose_food_spawn", ChooseFoodSpawn::STABLE_ID),
            ("calculate_food_effect", CalculateFoodEffect::STABLE_ID),
        ] {
            let entrypoint = parsed
                .nexa_functions
                .iter()
                .find(|entrypoint| entrypoint.name == name)
                .expect("declared typed entrypoint");
            assert_eq!(nexa::entrypoint_stable_id(entrypoint), expected);
        }
        let descriptor = nexa::abi_descriptor(&parsed);
        let fingerprint = descriptor.fingerprint.into_bytes();
        let descriptor: &'static [u8] = Box::leak(descriptor.bytes.into_boxed_slice());
        HostContract::new(
            "SnakeEntrypoints",
            CONTRACT_SOURCE,
            descriptor,
            fingerprint,
            nexa::contract_runtime_id(&parsed),
            nexa::prelude::HOST_CONTRACT_SCHEMA_VERSION,
        )
    })
}

fn policy(max_packages: usize) -> PackagePolicy {
    PackagePolicy {
        trust: TrustLevel::Trusted,
        capability_ceiling: CapabilitySet::default(),
        allowed_activation: ActivationSet::new([ActivationPolicy::DefaultEnabled]),
        max_packages,
        runtime_limits: PackageRuntimeLimits::default(),
        allow_entitlement: false,
    }
}

fn manifest(id: &str) -> String {
    format!(
        "schema = 2\n\
         kind = \"application\"\n\
         id = \"{id}\"\n\
         name = \"Entrypoint Test\"\n\
         version = \"1.0.0\"\n\
         source_root = \"src\"\n\
         entry = \"{id}\"\n\
         activation = \"default-enabled\"\n\
         handler_fuel = 20000\n\
         capabilities = []\n"
    )
}

fn package(id: &str, source: &str) -> MemoryPackage {
    MemoryPackage::new(id.replace('.', "-"), manifest(id))
        .source(format!("src/{}.nexa", id.replace('.', "/")), source)
}

fn source(packages: impl IntoIterator<Item = MemoryPackage>, max_packages: usize) -> MemorySource {
    let mut source = MemorySource::new(
        SourceId::new("snake-entrypoints").expect("Source ID"),
        policy(max_packages),
    );
    for package in packages {
        source = source.package(package);
    }
    source
}

fn engine_with_limit(
    packages: impl IntoIterator<Item = MemoryPackage>,
    max_packages: usize,
) -> NexaEngine {
    let contract = contract();
    let contract_runtime_id = contract.contract_runtime_id();
    NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(EmptyRegistry(contract_runtime_id)) as Box<dyn HostRegistry>
        })
        .package_source(source(packages, max_packages))
        .require_export::<OnEvent>()
        .build()
        .expect("typed entrypoint Engine")
}

fn engine(packages: impl IntoIterator<Item = MemoryPackage>) -> NexaEngine {
    engine_with_limit(packages, 8)
}

fn broadcast_count(engine: &mut NexaEngine, expected: usize) {
    let results = engine.dispatch::<OnEvent>(&10);
    assert_eq!(results.len(), expected);
    assert!(
        results.iter().all(Result::is_ok),
        "broadcast failures: {results:#?}"
    );
}

#[test]
fn snake_dispatch_scales_through_50_packages_and_zero() {
    let package_ids = (0..50)
        .map(|index| PackageId::new(format!("snake.scale{index:02}")).expect("scale Package ID"))
        .collect::<Vec<_>>();
    let packages = package_ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            package(
                id.as_str(),
                &format!("pub fn on_event(value: i32) -> i32 {{ return value + {index}; }}"),
            )
        })
        .collect::<Vec<_>>();
    let mut engine = engine_with_limit(packages, package_ids.len());

    engine.discover().expect("discover 50 Snake packages");
    engine.enable_defaults().expect("enable 50 Snake packages");
    broadcast_count(&mut engine, 50);

    for id in &package_ids[20..] {
        engine.disable(id).expect("disable package above 20");
    }
    broadcast_count(&mut engine, 20);

    for id in &package_ids[9..20] {
        engine.disable(id).expect("disable package above 9");
    }
    broadcast_count(&mut engine, 9);

    for id in &package_ids[..9] {
        engine.disable(id).expect("disable remaining package");
    }
    broadcast_count(&mut engine, 0);
    assert!(
        engine
            .inspection()
            .packages
            .iter()
            .all(|package| package.status == PackageStatus::Disabled)
    );

    let reloaded = &package_ids[49];
    engine.enable(reloaded).expect("re-enable one package");
    broadcast_count(&mut engine, 1);
    engine.reload(reloaded).expect("reload enabled package");
    broadcast_count(&mut engine, 1);
}

#[test]
fn snake_packages_use_required_broadcast_and_typed_optional_routing() {
    let spawn = package(
        "snake.spawn",
        "pub fn on_event(value: i32) -> i32 { return value + 1; }\n\
         pub fn choose_food_spawn(value: i32) -> i32 { return value + 7; }",
    );
    let food = package(
        "snake.food",
        "pub fn on_event(value: i32) -> i32 { return value + 2; }\n\
         pub fn calculate_food_effect(value: i32) -> i32 { return value + 100; }",
    );
    let mut engine = engine([spawn, food]);
    engine.discover().expect("discover Snake packages");
    engine
        .enable_defaults()
        .expect("missing Optional entrypoints are legal");

    let spawn_id = PackageId::new("snake.spawn").expect("spawn Package ID");
    let food_id = PackageId::new("snake.food").expect("food Package ID");

    assert!(engine.has_export::<OnEvent>(&spawn_id));
    assert!(engine.has_export::<OnEvent>(&food_id));
    assert!(engine.has_export::<ChooseFoodSpawn>(&spawn_id));
    assert!(!engine.has_export::<ChooseFoodSpawn>(&food_id));
    assert!(!engine.has_export::<CalculateFoodEffect>(&spawn_id));
    assert!(engine.has_export::<CalculateFoodEffect>(&food_id));

    let broadcast = engine
        .dispatch::<OnEvent>(&10)
        .into_iter()
        .map(|result| {
            let output = result.expect("required on_event call");
            (output.package_id.to_string(), output.value)
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        broadcast,
        BTreeMap::from([("snake.food".into(), 12), ("snake.spawn".into(), 11)])
    );

    assert_eq!(
        engine
            .call_optional::<ChooseFoodSpawn>(&spawn_id, &10)
            .expect("spawn provider implements choose_food_spawn")
            .expect("choose_food_spawn call")
            .value,
        17
    );
    assert!(
        engine
            .call_optional::<ChooseFoodSpawn>(&food_id, &10)
            .is_none()
    );
    assert_eq!(
        engine
            .call_optional::<CalculateFoodEffect>(&food_id, &5)
            .expect("food owner implements calculate_food_effect")
            .expect("calculate_food_effect call")
            .value,
        105
    );
    assert!(
        engine
            .call_optional::<CalculateFoodEffect>(&spawn_id, &5)
            .is_none()
    );

    let spawn_dispatch = engine.dispatch_optional::<ChooseFoodSpawn>(&20);
    assert_eq!(spawn_dispatch.len(), 1);
    let output = spawn_dispatch
        .into_iter()
        .next()
        .expect("one spawn provider")
        .expect("spawn dispatch");
    assert_eq!(output.package_id, spawn_id);
    assert_eq!(output.value, 27);

    let inspection = engine.inspection();
    let spawn = inspection
        .packages
        .iter()
        .find(|package| package.package_id == spawn_id)
        .expect("spawn inspection");
    assert_eq!(
        spawn.implemented_entrypoints,
        ["choose_food_spawn", "on_event"]
    );
    assert_eq!(spawn.required_entrypoints, ["on_event"]);
    assert!(spawn.missing_required_entrypoints.is_empty());
    assert_eq!(spawn.optional_entrypoint_signatures.len(), 1);
    assert_eq!(
        spawn.optional_entrypoint_signatures[0].name,
        "choose_food_spawn"
    );
    assert_eq!(
        spawn.optional_entrypoint_signatures[0].signature,
        ChooseFoodSpawn::SIGNATURE.into_owned()
    );
    assert_eq!(
        spawn.optional_entrypoint_signatures[0].effect,
        FunctionEffect::Ordinary
    );
}

#[test]
fn required_missing_and_optional_signature_mismatch_are_rejected() {
    let missing_required = package(
        "snake.missing",
        "pub fn choose_food_spawn(value: i32) -> i32 { return value; }",
    );
    let mut missing_engine = engine([missing_required]);
    missing_engine
        .discover()
        .expect("discover missing-required package");
    let missing_id = PackageId::new("snake.missing").expect("missing Package ID");
    assert!(missing_engine.enable(&missing_id).is_err());
    assert!(!missing_engine.has_export::<ChooseFoodSpawn>(&missing_id));

    let wrong_optional = package(
        "snake.wrong",
        "pub fn on_event(value: i32) -> i32 { return value; }\n\
         pub fn choose_food_spawn(value: bool) -> i32 { return 1; }",
    );
    let mut wrong_engine = engine([wrong_optional]);
    wrong_engine
        .discover()
        .expect("discover wrong-signature package");
    let wrong_id = PackageId::new("snake.wrong").expect("wrong Package ID");
    assert!(wrong_engine.enable(&wrong_id).is_err());
    assert!(!wrong_engine.has_export::<ChooseFoodSpawn>(&wrong_id));

    let wrong_effect = package(
        "snake.wrong_effect",
        "pub fn on_event(value: i32) -> i32 { return value; }\n\
         pub async fn choose_food_spawn(value: i32) -> i32 { return value; }",
    );
    let mut wrong_effect_engine = engine([wrong_effect]);
    wrong_effect_engine
        .discover()
        .expect("discover wrong-effect package");
    let wrong_effect_id = PackageId::new("snake.wrong_effect").expect("wrong-effect Package ID");
    assert!(wrong_effect_engine.enable(&wrong_effect_id).is_err());
    assert!(!wrong_effect_engine.has_export::<ChooseFoodSpawn>(&wrong_effect_id));
}

#[test]
fn required_marker_must_be_declared_by_the_contract() {
    let contract = contract();
    let contract_runtime_id = contract.contract_runtime_id();
    let result = NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(EmptyRegistry(contract_runtime_id)) as Box<dyn HostRegistry>
        })
        .require_export::<NotDeclared>()
        .build();
    assert!(matches!(result, Err(EngineError::Contract(_))));
}
