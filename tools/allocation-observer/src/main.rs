use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    HostArgs, HostCallOutcome, HostRegistry, HostTrap, PendingReason, PollResult, RealmConfig,
    RealmRuntime, ResourceContext, RuntimeHost, RuntimeHostDomain, RuntimeValue, StepConfig, TaskLimits,
};
use nexa_verifier::{VerifierLimits, verify};

struct CountingAllocator;

static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn observed(operation: impl FnOnce()) -> u64 {
    ALLOCATIONS.store(0, Ordering::SeqCst);
    ENABLED.store(true, Ordering::SeqCst);
    operation();
    ENABLED.store(false, Ordering::SeqCst);
    ALLOCATIONS.load(Ordering::SeqCst)
}

fn main() {
    let mut runs = Vec::new();
    for repeat in 1..=3 {
        let (mut realm, module) = make_realm(vec![
            Instruction::Safepoint,
            Instruction::Yield,
            Instruction::Return { source: 0 },
        ]);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let promotion = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Pending(PendingReason::ExplicitYield)
            ));
        });
        let resume = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });

        let (mut realm, module) = make_realm(vec![Instruction::Return { source: 0 }]);
        realm.set_trace_enabled(false);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let trace_off = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });

        let host = RuntimeHost::new(4);
        let (mut realm, module) = make_realm_with_host(host.clone());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        realm
            .create_resource_token(task, RuntimeHostDomain::Render)
            .unwrap();
        let pending = realm.create_host_request(task).unwrap();
        realm.wait_for_request(task, pending.request).unwrap();
        let realm_drop_transfer = observed(|| drop(realm));
        assert_eq!(host.pending_releases(), 2);
        drop(pending);
        runs.push((
            repeat,
            promotion,
            resume,
            trace_off,
            realm_drop_transfer,
        ));
    }

    let all_zero = runs
        .iter()
        .all(|(_, promotion, resume, trace_off, realm_drop_transfer)| {
            *promotion + *resume + *trace_off + *realm_drop_transfer == 0
        });
    println!(
        "{{\"observer\":\"global_allocator\",\"toolchain\":\"rustc-1.97.1\",\"runs\":[{}],\"all_hot_paths_zero\":{all_zero}}}",
        runs.iter()
            .map(|(repeat, promotion, resume, trace_off, realm_drop_transfer)| format!(
                "{{\"repeat\":{repeat},\"promotion\":{promotion},\"resume\":{resume},\"trace_off\":{trace_off},\"realm_drop_transfer\":{realm_drop_transfer}}}"
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(all_zero, "a measured runtime hot path allocated");
}

fn make_realm_with_host(
    host: RuntimeHost,
) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host_hash = StableId::from_name("allocation-observer-host");
    let schema = StableId::from_name("allocation-observer-schema");
    let verified = build_module(host_hash, schema, vec![Instruction::Return { source: 0 }]);
    let mut realm =
        RealmRuntime::hosted(RealmConfig::default(), host, Box::new(NoHost(host_hash))).unwrap();
    let module = realm.load_module(verified, host_hash, schema).unwrap();
    (realm, module)
}

fn make_realm(code: Vec<Instruction>) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host = StableId::from_name("allocation-observer-host");
    let schema = StableId::from_name("allocation-observer-schema");
    let verified = build_module(host, schema, code);
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm.load_module(verified, host, schema).unwrap();
    (realm, module)
}

fn build_module(
    host: StableId,
    schema: StableId,
    code: Vec<Instruction>,
) -> nexa_verifier::VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        2,
    );
    function.effect(FunctionEffect::Task);
    for instruction in code {
        function.emit(instruction);
    }
    let mut builder = ModuleBuilder::new();
    builder
        .metadata(host, schema)
        .function(function.finish().unwrap());
    verify(builder.finish(), VerifierLimits::default()).unwrap()
}

struct NoHost(StableId);

impl HostRegistry for NoHost {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        _: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::UnknownFunction(id))
    }
}
