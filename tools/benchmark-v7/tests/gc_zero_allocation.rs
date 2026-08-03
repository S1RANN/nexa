//! WP74 allocation authority: after heap construction and graph setup, a
//! complete incremental RootSnapshot/Mark/Sweep cycle performs zero system
//! allocations and zero reallocation bytes on the collector thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, ScriptExport, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    GcBudget, GcPhase, GcRoots, Heap, RealmConfig, RealmRuntime, RuntimeValue, StepConfig,
    TaskLimits,
};
use nexa_verifier::{VerifierLimits, verify};

struct CountingAllocator;

thread_local! {
    static ALLOCATION_CALLS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATION_BYTES: Cell<u64> = const { Cell::new(0) };
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATION_CALLS.try_with(|value| value.set(value.get().saturating_add(1)));
        let _ = ALLOCATION_BYTES
            .try_with(|value| value.set(value.get().saturating_add(layout.size() as u64)));
        // SAFETY: the allocation request is delegated unchanged to System.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATION_CALLS.try_with(|value| value.set(value.get().saturating_add(1)));
        let _ = ALLOCATION_BYTES
            .try_with(|value| value.set(value.get().saturating_add(layout.size() as u64)));
        // SAFETY: the allocation request is delegated unchanged to System.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the pointer/layout pair came from the delegated allocator.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATION_CALLS.try_with(|value| value.set(value.get().saturating_add(1)));
        let _ = ALLOCATION_BYTES
            .try_with(|value| value.set(value.get().saturating_add(new_size as u64)));
        // SAFETY: the pointer/layout pair and new size are delegated unchanged.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

fn allocation_snapshot() -> (u64, u64) {
    (
        ALLOCATION_CALLS.with(Cell::get),
        ALLOCATION_BYTES.with(Cell::get),
    )
}

fn reference_of(value: RuntimeValue) -> nexa_runtime::GcRef {
    match value {
        RuntimeValue::NamedRef { reference, .. } => reference,
        _ => panic!("expected a named heap reference"),
    }
}

fn set_map(heap: &mut Heap, map: RuntimeValue, key: RuntimeValue, value: RuntimeValue) {
    while heap
        .map_set(map, key, value)
        .expect("bounded map publication")
        == nexa_runtime::MapSetOutcome::RehashPending
    {}
}

fn yielding_root_module(host: StableId, export: StableId) -> nexa_verifier::VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::Ref],
            result: Some(ValueType::Ref),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .set_root(0)
        .expect("root map")
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let function = function.finish().expect("task function");
    let signature = function.signature.clone();
    let mut module = ModuleBuilder::new();
    let schema = nexa_bytecode::StateSchema::default().fingerprint();
    module.metadata(host, schema);
    let function = module.function(function);
    module.script_export(ScriptExport {
        stable_id: export,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("verified task module")
}

#[test]
fn incremental_gc_mark_and_sweep_allocate_zero_system_bytes() {
    let node = StableId::from_name("gc-zero-allocation::Node");
    let node_type = nexa_bytecode::ValueType::Named(node);
    let mut heap = Heap::new_with_arena_limits(128, 1 << 20, 1_024, 4_096, 512);

    let child = heap
        .allocate_class(node, &[RuntimeValue::I32(1)])
        .expect("reachable child");
    let array = heap
        .allocate_array(nexa_bytecode::array_type(node_type), node_type)
        .expect("reachable array");
    heap.array_push(array, child).expect("publish array child");
    let map = heap
        .allocate_map(
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, node_type),
            nexa_bytecode::ValueType::I32,
            node_type,
        )
        .expect("reachable map");
    set_map(&mut heap, map, RuntimeValue::I32(1), child);
    let root = heap
        .allocate_class(node, &[array, map])
        .expect("reachable root");

    // Condemned objects exercise String destruction, collection-arena
    // release, map-slot release, and free-slot publication during Sweep.
    heap.allocate_string("condemned-string-storage")
        .expect("condemned string");
    let condemned_array = heap
        .allocate_array(
            nexa_bytecode::array_type(nexa_bytecode::ValueType::I32),
            nexa_bytecode::ValueType::I32,
        )
        .expect("condemned array");
    for value in 0..16 {
        heap.array_push(condemned_array, RuntimeValue::I32(value))
            .expect("condemned element");
    }
    let condemned_map = heap
        .allocate_map(
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I32),
            nexa_bytecode::ValueType::I32,
            nexa_bytecode::ValueType::I32,
        )
        .expect("condemned map");
    for value in 0..8 {
        set_map(
            &mut heap,
            condemned_map,
            RuntimeValue::I32(value),
            RuntimeValue::I32(value * 2),
        );
    }

    let roots = GcRoots {
        running_frames: vec![reference_of(root)],
        ..GcRoots::default()
    };

    // Initialize the thread-local counters before the measured region.
    let _ = allocation_snapshot();
    let mut saw_mark = false;
    let mut saw_sweep = false;
    let mut completed = None;
    for _ in 0..1_024 {
        let before = allocation_snapshot();
        let report = heap
            .collect_incremental(&roots, GcBudget::objects(1))
            .expect("allocation-free GC slice");
        let after = allocation_snapshot();
        assert_eq!(
            after.0 - before.0,
            0,
            "{:?} slice performed a system allocation",
            report.phase
        );
        assert_eq!(
            after.1 - before.1,
            0,
            "{:?} slice allocated system bytes",
            report.phase
        );
        saw_mark |= report.phase == GcPhase::Mark;
        saw_sweep |= report.phase == GcPhase::Sweep || report.phase == GcPhase::Complete;
        if report.completed.is_some() {
            completed = report.completed;
            break;
        }
    }

    let completed = completed.expect("bounded incremental cycle completes");
    assert!(saw_mark);
    assert!(saw_sweep);
    assert_eq!(completed.live, 4, "root, array, map, and child survive");
    assert_eq!(heap.gc_phase(), GcPhase::Complete);
}

#[test]
fn realm_mark_and_sweep_reuse_the_root_snapshot_without_allocating() {
    let host = StableId::from_name("gc-zero-allocation::host");
    let export = StableId::from_name("gc-zero-allocation::yielding-root");
    let verified = yielding_root_module(host, export);
    let schema = verified.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig {
        max_heap_objects: 64,
        ..RealmConfig::default()
    });
    let module = realm
        .load_module(verified, host, schema)
        .expect("load root module");
    let scope = realm.create_scope(None).expect("scope");
    let root = realm
        .allocate(nexa_runtime::Object::String(String::from("task-root")))
        .expect("root string");
    let task = realm
        .spawn_task(
            module,
            export,
            &[RuntimeValue::Ref(root)],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 16,
                cumulative_budget: 128,
                limits: TaskLimits::default(),
            },
        )
        .expect("spawn root task");
    assert!(matches!(
        realm.poll_task(task, 16),
        Ok(nexa_runtime::TaskPoll::Yielded(_))
    ));

    // RootSnapshot runs exactly once and leaves the task root in the
    // preallocated gray queue. Subsequent Mark/Sweep slices must not rebuild
    // task or state root vectors.
    let first = realm
        .collect_garbage_incremental(GcBudget::objects(1))
        .expect("root snapshot");
    assert_eq!(first.phase, GcPhase::Mark);

    let _ = allocation_snapshot();
    let mut completed = None;
    for _ in 0..128 {
        let before = allocation_snapshot();
        let report = realm
            .collect_garbage_incremental(GcBudget::objects(1))
            .expect("allocation-free realm GC slice");
        let after = allocation_snapshot();
        assert_eq!(after.0 - before.0, 0);
        assert_eq!(after.1 - before.1, 0);
        if report.completed.is_some() {
            completed = report.completed;
            break;
        }
    }
    assert_eq!(completed.expect("cycle completes").live, 1);
    assert!(realm.resolve_heap_object(root).is_ok());
}
