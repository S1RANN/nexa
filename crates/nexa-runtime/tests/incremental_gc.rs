//! G1 incremental collector gate: budgeted `Idle -> Mark -> Sweep` cycles
//! must reclaim exactly what a full stop-the-world collection reclaims,
//! and the insertion barrier plus born-black allocation must keep every
//! reachable object alive under mutation during an active mark phase.

use nexa_core::StableId;
use nexa_runtime::{GcBudget, GcPhase, GcRoots, Heap, RuntimeValue};

fn class_type() -> StableId {
    StableId::from_name("incremental-gc::Node")
}

fn reference_of(value: RuntimeValue) -> nexa_runtime::GcRef {
    let RuntimeValue::NamedRef { reference, .. } = value else {
        panic!("class allocation returns a named reference");
    };
    reference
}

/// Runs budgeted steps until the cycle completes, panicking if it fails
/// to converge (a stuck phase machine would loop forever otherwise).
fn run_cycle(heap: &mut Heap, roots: &GcRoots, budget: usize) -> nexa_runtime::CollectionStats {
    for _ in 0..10_000 {
        let report = heap
            .collect_incremental(roots, GcBudget { max_steps: budget })
            .expect("incremental step");
        if let Some(stats) = report.completed {
            assert_eq!(heap.gc_phase(), GcPhase::Idle);
            return stats;
        }
    }
    panic!("incremental cycle did not converge");
}

#[test]
fn incremental_cycle_reclaims_exactly_what_full_collection_reclaims() {
    let build = |heap: &mut Heap| -> (GcRoots, Vec<nexa_runtime::GcRef>) {
        let node = class_type();
        // Reachable cluster: root class -> array -> class, plus a map.
        let leaf = heap
            .allocate_class(node, &[RuntimeValue::I32(1)])
            .expect("leaf");
        let leaf_reference = reference_of(leaf);
        let element = nexa_bytecode::ValueType::Named(node);
        let array = heap
            .allocate_array(nexa_bytecode::array_type(element), element)
            .expect("array");
        heap.array_push(array, leaf).expect("push leaf");
        let root = heap
            .allocate_class(node, &[array, RuntimeValue::I32(2)])
            .expect("root");
        let root_reference = reference_of(root);
        // Unreachable cluster: a two-node cycle plus a lone string.
        let cycle_a = heap
            .allocate_class(node, &[RuntimeValue::Unit])
            .expect("cycle a");
        let cycle_b = heap.allocate_class(node, &[cycle_a]).expect("cycle b");
        let cycle_a_reference = reference_of(cycle_a);
        heap.set_class_field(cycle_a, 0, cycle_b)
            .expect("close the cycle");
        heap.allocate_string("condemned").expect("lone string");
        let roots = GcRoots {
            running_frames: vec![root_reference],
            ..GcRoots::default()
        };
        (
            roots,
            vec![root_reference, leaf_reference, cycle_a_reference],
        )
    };

    let mut full = Heap::new(64);
    let (full_roots, _) = build(&mut full);
    let full_stats = full.collect(&full_roots).expect("full collection");

    let mut incremental = Heap::new(64);
    let (incremental_roots, probes) = build(&mut incremental);
    // Budget 1: every step does a single unit of work, maximizing the
    // number of interleaving points the cycle must survive.
    let incremental_stats = run_cycle(&mut incremental, &incremental_roots, 1);

    assert_eq!(incremental_stats.reclaimed, full_stats.reclaimed);
    assert_eq!(incremental_stats.live, full_stats.live);
    assert_eq!(incremental_stats.marked, full_stats.marked);
    // The reachable probes survive, the condemned cycle is gone.
    assert!(incremental.resolve(probes[0]).is_ok());
    assert!(incremental.resolve(probes[1]).is_ok());
    assert!(incremental.resolve(probes[2]).is_err());
}

#[test]
fn insertion_barrier_keeps_a_hidden_pointer_alive_during_mark() {
    let node = class_type();
    let mut heap = Heap::new(64);
    // Root class with two fields: [holder array, Unit].
    let hidden = heap
        .allocate_class(node, &[RuntimeValue::I32(41)])
        .expect("hidden node");
    let hidden_reference = reference_of(hidden);
    let element = nexa_bytecode::ValueType::Named(node);
    let holder = heap
        .allocate_array(nexa_bytecode::array_type(element), element)
        .expect("holder array");
    heap.array_push(holder, hidden).expect("hold hidden");
    let root = heap
        .allocate_class(node, &[holder, RuntimeValue::Unit])
        .expect("root");
    let root_reference = reference_of(root);
    let roots = GcRoots {
        running_frames: vec![root_reference],
        ..GcRoots::default()
    };
    // Step once with the smallest budget: the cycle enters Mark and only
    // the root is processed; `hidden` is still white.
    let report = heap
        .collect_incremental(&roots, GcBudget { max_steps: 1 })
        .expect("first mark step");
    assert_eq!(heap.gc_phase(), GcPhase::Mark);
    assert!(report.completed.is_none());
    // Classic hidden-pointer mutation: publish the white object into the
    // already-black root, then erase the gray path to it.
    heap.set_class_field(root, 1, hidden)
        .expect("publish hidden into black root");
    heap.array_set(holder, 0, RuntimeValue::I32(0))
        .expect("erase the gray path");
    let stats = run_cycle(&mut heap, &roots, 1);
    assert!(
        heap.resolve(hidden_reference).is_ok(),
        "insertion barrier must keep the hidden object alive"
    );
    assert_eq!(stats.reclaimed, 0, "everything stays reachable");
}

#[test]
fn objects_born_during_mark_and_sweep_survive_the_active_cycle() {
    let node = class_type();
    let mut heap = Heap::new(64);
    let root = heap
        .allocate_class(node, &[RuntimeValue::Unit])
        .expect("root");
    let root_reference = reference_of(root);
    let roots = GcRoots {
        running_frames: vec![root_reference],
        ..GcRoots::default()
    };
    heap.collect_incremental(&roots, GcBudget { max_steps: 1 })
        .expect("enter mark");
    assert_eq!(heap.gc_phase(), GcPhase::Mark);
    // Born during Mark, referencing a pre-existing white child: the
    // newborn is black and the child is shaded by the allocation barrier.
    let white_child = heap
        .allocate_class(node, &[RuntimeValue::I32(7)])
        .expect("white child");
    let born_mark = heap.allocate_class(node, &[white_child]).expect("newborn");
    let born_mark_reference = reference_of(born_mark);
    let white_child_reference = reference_of(white_child);
    // Drive the cycle into Sweep, then allocate again.
    let mut born_sweep_reference = None;
    for _ in 0..10_000 {
        let report = heap
            .collect_incremental(&roots, GcBudget { max_steps: 1 })
            .expect("step");
        if heap.gc_phase() == GcPhase::Sweep && born_sweep_reference.is_none() {
            let born_sweep = heap
                .allocate_class(node, &[RuntimeValue::I32(9)])
                .expect("sweep newborn");
            born_sweep_reference = Some(reference_of(born_sweep));
        }
        if report.completed.is_some() {
            break;
        }
    }
    assert!(
        heap.resolve(born_mark_reference).is_ok(),
        "mark-phase newborn survives"
    );
    assert!(
        heap.resolve(white_child_reference).is_ok(),
        "allocation barrier shades the newborn's children"
    );
    if let Some(reference) = born_sweep_reference {
        assert!(
            heap.resolve(reference).is_ok(),
            "sweep-phase newborn survives"
        );
    }
}

#[test]
fn full_collection_cancels_an_active_incremental_cycle() {
    let node = class_type();
    let mut heap = Heap::new(64);
    let root = heap
        .allocate_class(node, &[RuntimeValue::Unit])
        .expect("root");
    let root_reference = reference_of(root);
    heap.allocate_string("garbage").expect("condemned string");
    let roots = GcRoots {
        running_frames: vec![root_reference],
        ..GcRoots::default()
    };
    heap.collect_incremental(&roots, GcBudget { max_steps: 1 })
        .expect("enter mark");
    assert_eq!(heap.gc_phase(), GcPhase::Mark);
    let stats = heap.collect(&roots).expect("full collection mid-cycle");
    assert_eq!(heap.gc_phase(), GcPhase::Idle);
    assert_eq!(stats.reclaimed, 1);
    assert!(heap.resolve(root_reference).is_ok());
    // A fresh incremental cycle starts cleanly afterwards.
    let stats = run_cycle(&mut heap, &roots, 3);
    assert_eq!(stats.reclaimed, 0);
}

/// G3 bound gate: an adversarial graph where one object is referenced
/// hundreds of times (fan-in) plus a self-referential cycle must never
/// grow the preallocated gray queue - marks land at enqueue time, so
/// each object enters the queue at most once per cycle. The capacity
/// invariant is a debug assertion inside every incremental step.
#[test]
fn duplicate_heavy_graphs_never_outgrow_the_gray_queue() {
    let node = class_type();
    let mut heap = Heap::new_with_limits(64, usize::MAX, 1_024);
    let shared = heap
        .allocate_class(node, &[RuntimeValue::I32(1)])
        .expect("shared target");
    let shared_reference = reference_of(shared);
    let element = nexa_bytecode::ValueType::Named(node);
    let fan_in = heap
        .allocate_array(nexa_bytecode::array_type(element), element)
        .expect("fan-in array");
    for _ in 0..512 {
        heap.array_push(fan_in, shared).expect("push duplicate");
    }
    let looper = heap
        .allocate_class(node, &[RuntimeValue::Unit, fan_in])
        .expect("self loop");
    heap.set_class_field(looper, 0, looper)
        .expect("close the self reference");
    let roots = GcRoots {
        running_frames: vec![reference_of(looper)],
        ..GcRoots::default()
    };
    let stats = run_cycle(&mut heap, &roots, 1);
    assert_eq!(stats.reclaimed, 0);
    assert_eq!(stats.marked, 3, "shared, fan-in array, and looper");
    assert!(heap.resolve(shared_reference).is_ok());
    // A second cycle over the same graph behaves identically.
    let stats = run_cycle(&mut heap, &roots, 1);
    assert_eq!(stats.marked, 3);
}

/// G2 stress gate (`GC_V1.md`): 100,000 short-lived Class objects flow
/// through a small realm heap driven only by the water-mark trigger and
/// budgeted steps - ordinary gameplay never falls back to an explicit
/// full collection, and the heap never reaches capacity exhaustion.
#[test]
fn water_mark_trigger_sustains_one_hundred_thousand_short_lived_objects() {
    use nexa_runtime::{RealmConfig, RealmRuntime};
    let config = RealmConfig {
        max_heap_objects: 256,
        ..RealmConfig::default()
    };
    let mut realm = RealmRuntime::isolated(config);
    let node = class_type();
    let budget = GcBudget { max_steps: 32 };
    let mut triggered_steps = 0_u64;
    let mut completed_cycles = 0_u64;
    for index in 0..100_000_u32 {
        // Short-lived: the reference is dropped immediately; nothing roots
        // the object, so the next completed cycle reclaims it.
        let _ = realm
            .allocate(nexa_runtime::Object::Class {
                type_id: node,
                fields: [RuntimeValue::I32(i32::try_from(index % 1_000).expect("bounded"));
                    nexa_bytecode::MAX_CLASS_FIELDS],
                field_count: 1,
            })
            .expect("allocation never hits capacity under the trigger");
        if let Some(report) = realm
            .maybe_collect_garbage_incremental(budget)
            .expect("triggered step")
        {
            triggered_steps += 1;
            if report.completed.is_some() {
                completed_cycles += 1;
            }
        }
    }
    assert!(
        completed_cycles >= 2,
        "sustained allocation drives repeated cycles ({completed_cycles})"
    );
    assert!(
        triggered_steps > completed_cycles,
        "cycles span multiple budgeted steps"
    );
    // Shutdown-style full collection confirms nothing leaked beyond the
    // short-lived garbage still in flight.
    let final_stats = realm.collect_garbage().expect("final full collection");
    assert_eq!(final_stats.live, 0, "no short-lived object leaks");
}
