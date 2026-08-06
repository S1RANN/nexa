//! M5 stage-H gate (WP90/WP92): steady-state engine dispatch is
//! allocation-exact. Generated signatures and argument blocks are static
//! or inline, and the caller reuses a bounded output buffer. After warmup,
//! broadcasts, provider/owner calls, and idle ticks perform zero system
//! allocations while lifecycle changes still rebuild the cached plan.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::OnceLock;

use nexa::prelude::{
    FunctionEffect, HostCallOutcome, HostFunctionSlot, HostRegistry, HostTrap,
    ResolvedHostFunction, ResourceContext, RuntimeHostArgs, RuntimeValue,
    ScriptArgumentRequirements, ScriptArguments, ScriptCallError, ScriptCallWriter, ScriptExport,
    ScriptOutputReader, ScriptSignature, StableId, ValueType,
};
use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilitySet, HostContract, MemoryPackage, MemorySource,
    NexaEngine, PackageId, PackagePolicy, PackageRuntimeLimits, SourceId, TrustLevel,
};

// --- The counting allocator -------------------------------------------------
//
// Counts are kept per thread so background workers (development compiles,
// etc.) can never inject noise into a measurement taken on the test thread.

struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.realloc(pointer, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocations_during<T>(work: impl FnOnce() -> T) -> (u64, T) {
    let before = ALLOCATIONS.with(Cell::get);
    let value = work();
    (ALLOCATIONS.with(Cell::get) - before, value)
}

fn write_allocation_receipt(cases: [(&str, u64); 6]) {
    let Some(path) = std::env::var_os("NEXA_M5_STEADY_STATE_RECEIPT") else {
        return;
    };
    let implementation_commit = std::env::var("NEXA_M5_IMPLEMENTATION_COMMIT")
        .expect("receipt generation requires the implementation commit");
    let test_source_hash = std::env::var("NEXA_M5_STEADY_STATE_SOURCE_HASH")
        .expect("receipt generation requires the test source hash");
    let max_system_allocations = cases
        .iter()
        .map(|(_, allocations)| *allocations)
        .max()
        .unwrap_or(0);
    let cases = cases
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    let report = serde_json::json!({
        "schema": 1,
        "report": "Nexa M5 WP92 Steady-State Engine Allocation",
        "implementation_commit": implementation_commit,
        "test_source_hash": test_source_hash,
        "cases": cases,
        "max_system_allocations": max_system_allocations,
        "status": if max_system_allocations == 0 { "PASS" } else { "FAIL" },
    });
    let path = std::path::PathBuf::from(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create receipt directory");
    }
    let mut rendered = serde_json::to_vec_pretty(&report).expect("serialize allocation receipt");
    rendered.push(b'\n');
    std::fs::write(path, rendered).expect("write allocation receipt");
}

// --- Contract and entrypoint markers ----------------------------------------

const CONTRACT_SOURCE: &str = r"contract SnakeEntrypoints {
    host {}

    nexa {
        fn on_event(value: i32) -> i32;
        fn choose_food_spawn(value: i32) -> i32;
        fn calculate_food_effect(value: i32) -> i32;
    }
}";

macro_rules! i32_entrypoint {
    ($marker:ident, $name:literal, $stable_id:literal, $contract_slot:expr, $effect:ident) => {
        struct $marker;

        impl ScriptExport for $marker {
            type Args = i32;
            type Output = i32;

            const STABLE_ID: StableId = StableId($stable_id);
            const NAME: &'static str = $name;
            const CONTRACT_SLOT: usize = $contract_slot;
            const SIGNATURE: ScriptSignature =
                ScriptSignature::new(&[ValueType::I32], Some(ValueType::I32));
            const EFFECT: FunctionEffect = FunctionEffect::$effect;

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

// NIDL v2 declaration identities for the validated `SnakeEntrypoints`
// Contract; `contract()` re-derives and asserts them from the parsed NIDL.
// The markers declare Ordinary - exactly what generated bindings produce
// for a sync contract function. The packages strengthen
// `calculate_food_effect` to `@immediate`, so the engine's module-effect
// routing (WP89) must send it down the task-free path on its own.
i32_entrypoint!(OnEvent, "on_event", 0xefff_24e2_9dbd_2cb4, 2, Ordinary);
i32_entrypoint!(
    ChooseFoodSpawn,
    "choose_food_spawn",
    0x0418_ea07_84bf_08cf,
    1,
    Ordinary
);
i32_entrypoint!(
    CalculateFoodEffect,
    "calculate_food_effect",
    0xe9ac_e9e4_2957_0132,
    0,
    Ordinary
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
        let parsed = nexa::parse_contract(CONTRACT_SOURCE).expect("steady-state Contract");
        for (name, expected) in [
            ("on_event", OnEvent::STABLE_ID),
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

fn policy() -> PackagePolicy {
    PackagePolicy {
        trust: TrustLevel::Trusted,
        capability_ceiling: CapabilitySet::default(),
        allowed_activation: ActivationSet::new([ActivationPolicy::DefaultEnabled]),
        max_packages: 8,
        runtime_limits: PackageRuntimeLimits::default(),
        allow_entitlement: false,
    }
}

fn manifest(id: &str) -> String {
    format!(
        "schema = 2\n\
         kind = \"application\"\n\
         id = \"{id}\"\n\
         name = \"Steady State Test\"\n\
         version = \"1.0.0\"\n\
         source_root = \"src\"\n\
         entry = \"{id}\"\n\
         activation = \"default-enabled\"\n\
         handler_fuel = 20000\n\
         capabilities = []\n"
    )
}

/// Every package implements the required Ordinary `on_event` plus the
/// `@immediate` broadcast target with a package-distinct constant.
fn package(id: &str, offset: i32) -> MemoryPackage {
    let source = format!(
        "pub fn on_event(value: i32) -> i32 {{ return value + {offset}; }}\n\
         @immediate\n\
         pub fn choose_food_spawn(value: i32) -> i32 {{ return value + 10 + {offset}; }}\n\
         @immediate\n\
         pub fn calculate_food_effect(value: i32) -> i32 {{ return value * 2 + {offset}; }}\n"
    );
    MemoryPackage::new(id.replace('.', "-"), manifest(id))
        .source(format!("src/{}.nexa", id.replace('.', "/")), source)
}

fn engine(packages: impl IntoIterator<Item = MemoryPackage>) -> NexaEngine {
    let contract = contract();
    let contract_runtime_id = contract.contract_runtime_id();
    let mut source = MemorySource::new(
        SourceId::new("steady-state-alloc").expect("Source ID"),
        policy(),
    );
    for package in packages {
        source = source.package(package);
    }
    let mut engine = NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(EmptyRegistry(contract_runtime_id)) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .require_export::<OnEvent>()
        .build()
        .expect("steady-state Engine");
    engine.discover().expect("discover packages");
    engine.enable_defaults().expect("enable packages");
    let statuses = engine
        .packages()
        .iter()
        .map(|package| (package.id.to_string(), package.status))
        .collect::<Vec<_>>();
    assert!(
        statuses
            .iter()
            .all(|(_, status)| *status == nexa_embed::PackageStatus::Enabled),
        "every package must enable; statuses: {statuses:?}; diagnostics: {:?}",
        engine.diagnostics()
    );
    engine
}

fn broadcast_values(engine: &mut NexaEngine, args: i32) -> Vec<(String, i32)> {
    engine
        .dispatch::<CalculateFoodEffect>(&args)
        .into_iter()
        .map(|result| {
            let output = result.expect("immediate broadcast call");
            (output.package_id.to_string(), output.value)
        })
        .collect()
}

#[test]
fn steady_state_engine_paths_allocate_nothing() {
    let mut engine = engine([
        package("pkg.alpha", 1),
        package("pkg.beta", 2),
        package("pkg.gamma", 3),
    ]);

    // Result parity first: deterministic broadcast order (equal priority,
    // package id ascending) with per-package immediate results.
    assert_eq!(
        broadcast_values(&mut engine, 10),
        vec![
            ("pkg.alpha".to_string(), 21),
            ("pkg.beta".to_string(), 22),
            ("pkg.gamma".to_string(), 23),
        ]
    );

    let mut outputs = Vec::with_capacity(3);
    let mut undersized = Vec::with_capacity(2);
    let capacity_error = engine
        .dispatch_into::<CalculateFoodEffect>(&0, &mut undersized)
        .expect_err("undersized dispatch buffer must fail before calling handlers");
    assert_eq!(capacity_error.required, 3);
    assert_eq!(capacity_error.capacity, 2);
    assert!(undersized.is_empty());

    // Warmup: fill the WP90 plan, the H1 continuation pools, fixed output
    // storage, tick metrics, and every lazy one-time initialization.
    for round in 0..4 {
        let outputs = engine
            .dispatch_into::<CalculateFoodEffect>(&round, &mut outputs)
            .expect("bounded dispatch output");
        assert_eq!(outputs.len(), 3);
    }
    let alpha = PackageId::new("pkg.alpha").expect("package id");
    engine
        .call::<CalculateFoodEffect>(&alpha, &5)
        .expect("warm single call");
    engine
        .call_optional::<ChooseFoodSpawn>(&alpha, &5)
        .expect("provider implements choose_food_spawn")
        .expect("warm provider call");
    let mut optional_outputs = Vec::with_capacity(3);
    assert_eq!(
        engine
            .dispatch_optional_into::<ChooseFoodSpawn>(&5, &mut optional_outputs)
            .expect("bounded Optional dispatch output")
            .len(),
        3
    );
    for _ in 0..4 {
        engine.tick().expect("warm idle Engine tick");
    }

    // WP92 hard gate: the reusable output storage, static signatures,
    // fixed argument blocks, cached plan, shared identities/capabilities,
    // and task-free immediate path allocate nothing.
    let rounds = 8_u64;
    let (dispatch_allocations, _) = allocations_during(|| {
        for round in 0..rounds {
            #[allow(clippy::cast_possible_truncation)]
            let current = engine
                .dispatch_into::<CalculateFoodEffect>(&(round as i32), &mut outputs)
                .expect("bounded dispatch output");
            assert_eq!(current.len(), 3);
            for output in current {
                assert!(output.is_ok(), "steady-state immediate call");
            }
        }
    });
    assert_eq!(
        dispatch_allocations, 0,
        "steady-state dispatch must perform zero system allocations"
    );

    let (projected_dispatch_allocations, _) = allocations_during(|| {
        for _ in 0..rounds {
            let current = engine
                .dispatch_with_into::<CalculateFoodEffect>(
                    |package| i32::from(package.id.as_str() == "pkg.alpha"),
                    &mut outputs,
                )
                .expect("bounded projected dispatch output");
            assert_eq!(current.len(), 3);
            assert!(current.iter().all(Result::is_ok));
        }
    });
    assert_eq!(
        projected_dispatch_allocations, 0,
        "package-projected dispatch must perform zero system allocations"
    );

    let (optional_dispatch_allocations, _) = allocations_during(|| {
        for _ in 0..rounds {
            let current = engine
                .dispatch_optional_into::<ChooseFoodSpawn>(&7, &mut optional_outputs)
                .expect("bounded Optional dispatch output");
            assert_eq!(current.len(), 3);
            assert!(current.iter().all(Result::is_ok));
        }
    });
    assert_eq!(
        optional_dispatch_allocations, 0,
        "Optional broadcast must perform zero system allocations"
    );

    let (call_allocations, called) =
        allocations_during(|| engine.call::<CalculateFoodEffect>(&alpha, &7));
    assert_eq!(called.expect("steady-state single call").value, 15);
    assert_eq!(
        call_allocations, 0,
        "a steady-state owner call must perform zero system allocations"
    );

    let (provider_allocations, provided) =
        allocations_during(|| engine.call_optional::<ChooseFoodSpawn>(&alpha, &7));
    assert_eq!(
        provided
            .expect("provider implements choose_food_spawn")
            .expect("steady-state provider call")
            .value,
        18
    );
    assert_eq!(
        provider_allocations, 0,
        "a steady-state optional provider call must perform zero system allocations"
    );

    let mut per_tick_allocations = [0_u64; 8];
    for allocation_count in &mut per_tick_allocations {
        let (allocations, report) =
            allocations_during(|| engine.tick().expect("steady-state idle Engine tick"));
        *allocation_count = allocations;
        assert!(report.development_events.is_empty());
        assert!(report.diagnostics.is_empty());
        assert!(report.reloads.is_empty());
        assert!(report.faulted_packages.is_empty());
        assert_eq!(report.released_resources, 0);
    }
    assert_eq!(
        per_tick_allocations, [0; 8],
        "a steady-state idle Engine tick must perform zero system allocations"
    );
    write_allocation_receipt([
        ("broadcast", dispatch_allocations),
        ("projected_broadcast", projected_dispatch_allocations),
        ("optional_broadcast", optional_dispatch_allocations),
        ("owner_call", call_allocations),
        ("optional_provider_call", provider_allocations),
        (
            "idle_tick",
            per_tick_allocations.into_iter().max().unwrap_or(0),
        ),
    ]);
}

#[test]
fn dispatch_plan_follows_lifecycle_changes() {
    let mut engine = engine([
        package("pkg.alpha", 1),
        package("pkg.beta", 2),
        package("pkg.gamma", 3),
    ]);
    let beta = PackageId::new("pkg.beta").expect("package id");
    let mut optional_outputs = Vec::with_capacity(3);

    // Warm plan with all three packages.
    assert_eq!(broadcast_values(&mut engine, 1).len(), 3);
    assert_eq!(
        engine
            .dispatch_optional_into::<ChooseFoodSpawn>(&1, &mut optional_outputs)
            .expect("warm Optional dispatch plan")
            .len(),
        3
    );

    // Disabling a package must invalidate the cached plan: the O(n)
    // revalidation scan sees the population change and rebuilds.
    engine.disable(&beta).expect("disable pkg.beta");
    assert_eq!(
        broadcast_values(&mut engine, 1),
        vec![("pkg.alpha".to_string(), 3), ("pkg.gamma".to_string(), 5),]
    );
    assert_eq!(
        engine
            .dispatch_optional_into::<ChooseFoodSpawn>(&1, &mut optional_outputs)
            .expect("Optional plan after disable")
            .len(),
        2
    );

    // Re-enabling restores the full deterministic broadcast order.
    engine.enable(&beta).expect("re-enable pkg.beta");
    assert_eq!(
        broadcast_values(&mut engine, 1),
        vec![
            ("pkg.alpha".to_string(), 3),
            ("pkg.beta".to_string(), 4),
            ("pkg.gamma".to_string(), 5),
        ]
    );
    assert_eq!(
        engine
            .dispatch_optional_into::<ChooseFoodSpawn>(&1, &mut optional_outputs)
            .expect("Optional plan after re-enable")
            .len(),
        3
    );
}
