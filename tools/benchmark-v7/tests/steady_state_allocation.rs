//! M5 stage-H gate (WP90/WP92): steady-state engine dispatch is
//! allocation-exact. After warmup, the engine and runtime infrastructure
//! allocate nothing per broadcast - no plan rebuild, no identity or
//! capability-set clones, no task lifecycle, no continuation storage -
//! leaving only the caller-visible outputs vector plus, per called
//! package, the two vectors the `ScriptExport` trait contract itself
//! returns by value (`encode_args` argument registers and the
//! `signature()` used by the realm's per-call safety re-validation).
//! The WP90 dispatch plan is reused across broadcasts - its revalidation
//! scan allocates nothing - while lifecycle changes still rebuild it
//! correctly.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::OnceLock;

use nexa::prelude::{
    FunctionEffect, HostCallOutcome, HostRegistry, HostTrap, ResourceContext, RuntimeHostArgs,
    RuntimeValue, ScriptArgumentRequirements, ScriptCallError, ScriptCallWriter, ScriptExport,
    ScriptOutputReader, Signature, StableId, ValueType,
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
    ($marker:ident, $name:literal, $stable_id:literal, $effect:ident) => {
        struct $marker;

        impl ScriptExport for $marker {
            type Args = i32;
            type Output = i32;

            const STABLE_ID: StableId = StableId($stable_id);
            const NAME: &'static str = $name;

            fn signature() -> Signature {
                Signature {
                    parameters: vec![ValueType::I32],
                    result: Some(ValueType::I32),
                }
            }

            fn effect() -> FunctionEffect {
                FunctionEffect::$effect
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
    };
}

// NIDL v2 declaration identities for the validated `SnakeEntrypoints`
// Contract; `contract()` re-derives and asserts them from the parsed NIDL.
// Both markers declare Ordinary - exactly what generated bindings produce
// for a sync contract function. The packages strengthen
// `calculate_food_effect` to `@immediate`, so the engine's module-effect
// routing (WP89) must send it down the task-free path on its own.
i32_entrypoint!(OnEvent, "on_event", 0xefff_24e2_9dbd_2cb4, Ordinary);
i32_entrypoint!(
    CalculateFoodEffect,
    "calculate_food_effect",
    0xe9ac_e9e4_2957_0132,
    Ordinary
);

struct EmptyRegistry(StableId);

impl HostRegistry for EmptyRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call_runtime(
        &mut self,
        id: StableId,
        _: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::UnknownFunction(id))
    }
}

fn contract() -> HostContract {
    static CONTRACT: OnceLock<HostContract> = OnceLock::new();
    *CONTRACT.get_or_init(|| {
        let parsed = nexa::parse_nidl(CONTRACT_SOURCE).expect("steady-state Contract");
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
fn steady_state_dispatch_allocates_only_the_visible_vectors() {
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

    // Warmup: fill the WP90 plan, the H1 continuation pools, and every
    // lazy one-time initialization on the call path.
    for round in 0..4 {
        let outputs = engine.dispatch::<CalculateFoodEffect>(&round);
        assert_eq!(outputs.len(), 3);
    }
    let alpha = PackageId::new("pkg.alpha").expect("package id");
    engine
        .call::<CalculateFoodEffect>(&alpha, &5)
        .expect("warm single call");

    // WP92 gate: per broadcast, exactly one outputs vector plus the two
    // per-package trait-contract vectors (argument registers + the
    // signature the realm re-validates against) - nothing else. Plan
    // rebuilds, identity clones, capability sets, pooled continuations,
    // and the task-free immediate path (WP89) all stay allocation-free.
    const TRAIT_CONTRACT_VECTORS_PER_CALL: u64 = 2;
    let rounds = 8_u64;
    let (dispatch_allocations, _) = allocations_during(|| {
        for round in 0..rounds {
            #[allow(clippy::cast_possible_truncation)]
            let outputs = engine.dispatch::<CalculateFoodEffect>(&(round as i32));
            assert_eq!(outputs.len(), 3);
            for output in outputs {
                output.expect("steady-state immediate call");
            }
        }
    });
    assert_eq!(
        dispatch_allocations,
        rounds * (1 + 3 * TRAIT_CONTRACT_VECTORS_PER_CALL),
        "steady-state dispatch must allocate exactly the outputs vector plus \
         the per-package trait-contract vectors"
    );

    // A steady-state single call allocates only its two trait-contract
    // vectors; the engine and the task-free immediate path add nothing.
    let (call_allocations, called) =
        allocations_during(|| engine.call::<CalculateFoodEffect>(&alpha, &7));
    assert_eq!(called.expect("steady-state single call").value, 15);
    assert_eq!(
        call_allocations, TRAIT_CONTRACT_VECTORS_PER_CALL,
        "a steady-state immediate call must allocate exactly its trait-contract vectors"
    );
}

#[test]
fn dispatch_plan_follows_lifecycle_changes() {
    let mut engine = engine([
        package("pkg.alpha", 1),
        package("pkg.beta", 2),
        package("pkg.gamma", 3),
    ]);
    let beta = PackageId::new("pkg.beta").expect("package id");

    // Warm plan with all three packages.
    assert_eq!(broadcast_values(&mut engine, 1).len(), 3);

    // Disabling a package must invalidate the cached plan: the O(n)
    // revalidation scan sees the population change and rebuilds.
    engine.disable(&beta).expect("disable pkg.beta");
    assert_eq!(
        broadcast_values(&mut engine, 1),
        vec![("pkg.alpha".to_string(), 3), ("pkg.gamma".to_string(), 5),]
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
}
