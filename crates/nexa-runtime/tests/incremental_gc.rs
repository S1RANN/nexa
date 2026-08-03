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
            .collect_incremental(roots, GcBudget::objects(budget))
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
        .collect_incremental(&roots, GcBudget::objects(1))
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
    heap.collect_incremental(&roots, GcBudget::objects(1))
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
            .collect_incremental(&roots, GcBudget::objects(1))
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
    heap.collect_incremental(&roots, GcBudget::objects(1))
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
    let budget = GcBudget::objects(32);
    let mut triggered_steps = 0_u64;
    let mut completed_cycles = 0_u64;
    for index in 0..100_000_u32 {
        // Short-lived: the reference is dropped immediately; nothing roots
        // the object, so the next completed cycle reclaims it.
        let _ = realm
            .allocate_class(
                node,
                &[RuntimeValue::I32(
                    i32::try_from(index % 1_000).expect("bounded"),
                )],
            )
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

/// Builds one condemned object of every byte-carrying kind (string, array
/// with a live extent, map with entries, and a class with an arena-backed
/// field extent.
fn build_byte_graph(heap: &mut Heap) {
    let node = class_type();
    heap.allocate_string("byte-accounting-corpus")
        .expect("condemned string");
    let element = nexa_bytecode::ValueType::I32;
    let array = heap
        .allocate_array(nexa_bytecode::array_type(element), element)
        .expect("condemned array");
    for index in 0..9 {
        heap.array_push(array, RuntimeValue::I32(index))
            .expect("array element");
    }
    let map = heap
        .allocate_map(nexa_bytecode::map_type(element, element), element, element)
        .expect("condemned map");
    for index in 0..5 {
        heap.map_set(map, RuntimeValue::I32(index), RuntimeValue::I32(index * 2))
            .expect("map entry");
    }
    heap.allocate_class(node, &[RuntimeValue::I32(7)])
        .expect("condemned class");
}

/// G4 gate: the byte-accounting symmetry contract. The categorized
/// inspection taken before collection predicts exactly the payload bytes
/// the sweep reports as reclaimed - for the full collector, for the
/// incremental cycle latch, and for the per-step report sum.
#[test]
fn reclaimed_bytes_match_the_inspection_across_full_and_incremental() {
    let roots = GcRoots::default();

    let mut full = Heap::new(64);
    build_byte_graph(&mut full);
    let before = full.byte_inspection();
    let expected = before.string_bytes
        + before.array_bytes
        + before.buffer_bytes
        + before.map_bytes
        + before.class_payload_bytes;
    assert!(expected > 0, "the corpus owns out-of-slot bytes");
    assert!(
        before.class_payload_bytes > 0,
        "class field extent is visible as an out-of-slot category"
    );
    let stats = full.collect(&roots).expect("full collection");
    assert_eq!(stats.live, 0);
    assert_eq!(
        full.last_cycle_bytes_reclaimed(),
        expected,
        "full sweep reclaims exactly the inspected payload bytes"
    );
    let after = full.byte_inspection();
    assert_eq!(
        after.string_bytes
            + after.array_bytes
            + after.buffer_bytes
            + after.map_bytes
            + after.class_payload_bytes,
        0,
        "no payload bytes survive an empty-root collection"
    );
    assert!(
        after.allocator_slack_bytes > before.allocator_slack_bytes,
        "reclaimed arena extents and vacated slots return to slack"
    );

    let mut incremental = Heap::new(64);
    build_byte_graph(&mut incremental);
    let mut step_bytes = 0_u64;
    let mut completed = false;
    for _ in 0..10_000 {
        let report = incremental
            .collect_incremental(&roots, GcBudget::objects(1))
            .expect("incremental step");
        step_bytes += report.bytes_reclaimed;
        if report.completed.is_some() {
            completed = true;
            break;
        }
    }
    assert!(completed, "incremental cycle converges");
    assert_eq!(
        incremental.last_cycle_bytes_reclaimed(),
        expected,
        "incremental cycle latch matches the full collector byte for byte"
    );
    assert_eq!(
        step_bytes, expected,
        "per-step reports sum to the cycle total"
    );
}

/// G5 gate: the byte axis slices sweeps at object-payload granularity -
/// with `max_bytes: 1` no step reclaims more than one byte-carrying
/// object - while the cycle still reclaims exactly what the unbudgeted
/// collector reclaims.
#[test]
fn byte_budget_slices_the_sweep_without_changing_the_outcome() {
    let roots = GcRoots::default();
    let mut reference = Heap::new(64);
    build_byte_graph(&mut reference);
    let expected_bytes = {
        let inspection = reference.byte_inspection();
        inspection.string_bytes
            + inspection.array_bytes
            + inspection.buffer_bytes
            + inspection.map_bytes
            + inspection.class_payload_bytes
    };
    let expected_stats = reference.collect(&roots).expect("reference collection");

    let mut budgeted = Heap::new(64);
    build_byte_graph(&mut budgeted);
    let budget = GcBudget {
        max_objects: usize::MAX,
        max_bytes: 1,
        max_duration: std::time::Duration::MAX,
    };
    let mut total_bytes = 0_u64;
    let mut byte_carrying_steps = 0_u32;
    let mut completed = None;
    for _ in 0..10_000 {
        let report = budgeted
            .collect_incremental(&roots, budget)
            .expect("byte-budgeted step");
        total_bytes += report.bytes_reclaimed;
        if report.bytes_reclaimed > 0 {
            byte_carrying_steps += 1;
            assert!(
                report.bytes_reclaimed < expected_bytes,
                "a one-byte budget must split the byte-carrying reclamations"
            );
        }
        if let Some(stats) = report.completed {
            completed = Some(stats);
            break;
        }
    }
    let stats = completed.expect("byte-budgeted cycle converges");
    assert_eq!(stats.reclaimed, expected_stats.reclaimed);
    assert_eq!(total_bytes, expected_bytes);
    assert!(
        byte_carrying_steps >= 3,
        "string, array extent, and map storage land in separate steps"
    );
}

/// G5 gate: a zero wall-clock budget still makes progress - the
/// first-unit guarantee turns it into single-unit steps - and the cycle
/// converges to the reference outcome. `Instant::now() < now` is false on
/// any clock, so this is deterministic despite reading time.
#[test]
fn zero_duration_budget_degrades_to_single_unit_steps() {
    let roots = GcRoots::default();
    let mut reference = Heap::new(64);
    build_byte_graph(&mut reference);
    let expected_stats = reference.collect(&roots).expect("reference collection");
    let expected_bytes = reference.last_cycle_bytes_reclaimed();

    let mut budgeted = Heap::new(64);
    build_byte_graph(&mut budgeted);
    let budget = GcBudget {
        max_objects: usize::MAX,
        max_bytes: u64::MAX,
        max_duration: std::time::Duration::ZERO,
    };
    let mut completed = None;
    let mut steps_taken = 0_u32;
    for _ in 0..10_000 {
        let report = budgeted
            .collect_incremental(&roots, budget)
            .expect("zero-duration step");
        steps_taken += 1;
        assert!(
            report.objects_marked + report.slots_swept <= 1,
            "an expired deadline admits exactly the guaranteed unit"
        );
        if let Some(stats) = report.completed {
            completed = Some(stats);
            break;
        }
    }
    let stats = completed.expect("zero-duration cycle converges");
    assert_eq!(stats.reclaimed, expected_stats.reclaimed);
    assert_eq!(budgeted.last_cycle_bytes_reclaimed(), expected_bytes);
    assert!(
        steps_taken >= u32::try_from(stats.reclaimed).expect("bounded"),
        "single-unit steps need at least one call per swept slot"
    );
}

/// A zero object budget performs no work and leaves the phase untouched.
#[test]
fn zero_object_budget_is_a_no_op() {
    let mut heap = Heap::new(16);
    build_byte_graph(&mut heap);
    let report = heap
        .collect_incremental(&GcRoots::default(), GcBudget::objects(0))
        .expect("no-op step");
    assert_eq!(report, nexa_runtime::IncrementalGcReport::default());
    assert_eq!(heap.gc_phase(), GcPhase::Idle);
}

/// G6 gate: the live payload gauge tracks the byte inspection through
/// allocation and collection. (The debug drift assertion inside every
/// full collection additionally pins the gauge across the whole suite.)
#[test]
fn live_payload_gauge_matches_the_inspection_across_the_lifecycle() {
    let mut heap = Heap::new(64);
    assert_eq!(heap.live_payload_bytes(), 0);
    build_byte_graph(&mut heap);
    let inspection = heap.byte_inspection();
    assert_eq!(
        heap.live_payload_bytes(),
        inspection.string_bytes
            + inspection.array_bytes
            + inspection.buffer_bytes
            + inspection.map_bytes
            + inspection.class_payload_bytes,
        "the gauge equals the inspected out-of-slot payload"
    );
    heap.collect(&GcRoots::default()).expect("full collection");
    assert_eq!(
        heap.live_payload_bytes(),
        0,
        "reclaiming every object empties the gauge"
    );
}

/// K5 gate: class fields occupy an exact arena extent, contribute once to
/// the live-payload gauge, and no longer widen every object slot to the
/// maximum inline field array.
#[test]
fn compact_class_fields_are_arena_backed_and_charged_once() {
    let mut heap = Heap::new(8);
    let class = heap
        .allocate_class(class_type(), &[RuntimeValue::I32(7)])
        .expect("class allocation");
    let reference = reference_of(class);
    let field_bytes = u64::try_from(std::mem::size_of::<RuntimeValue>()).expect("value size");
    let inspection = heap.byte_inspection();

    assert_eq!(
        heap.object_fields(reference),
        Ok(&[RuntimeValue::I32(7)][..])
    );
    assert_eq!(inspection.class_payload_bytes, field_bytes);
    assert_eq!(
        heap.live_payload_bytes(),
        field_bytes,
        "the extent is charged by commit exactly once"
    );
    assert!(
        inspection.object_header_bytes <= field_bytes.saturating_mul(2),
        "the compact slot must stay within two RuntimeValue cells instead of the removed \
         maximum-width field array"
    );
    assert_eq!(
        inspection.total(),
        inspection
            .object_header_bytes
            .saturating_add(inspection.class_payload_bytes)
            .saturating_add(inspection.allocator_slack_bytes)
            .saturating_add(inspection.profiler_bytes),
        "class payload is an exclusive category in the byte total"
    );

    heap.collect(&GcRoots::default())
        .expect("unrooted class collection");
    assert_eq!(heap.live_payload_bytes(), 0);
    assert_eq!(heap.byte_inspection().class_payload_bytes, 0);
}

/// G6 gate: `max_heap_bytes` refuses growth past the ceiling at the
/// allocation boundary with `CapacityExhausted`, and collection restores
/// headroom.
#[test]
fn heap_byte_ceiling_refuses_growth_until_collection_frees_it() {
    let mut heap = Heap::new(16);
    heap.set_max_heap_bytes(32);
    heap.allocate_string("0123456789abcdef")
        .expect("a 16-byte string fits under the 32-byte ceiling");
    let refused = heap.allocate_string("0123456789abcdef0123456789abcdef");
    assert_eq!(
        refused.unwrap_err(),
        nexa_runtime::HeapError::CapacityExhausted,
        "16 + 32 bytes exceeds the ceiling"
    );
    heap.collect(&GcRoots::default()).expect("full collection");
    heap.allocate_string("0123456789abcdef0123456789abcdef")
        .expect("the reclaimed heap has headroom again");
}

/// G6 gate: amortized array growth respects the byte ceiling - the
/// conservative regrow admission (old extent still held) refuses the
/// relocation, existing contents stay intact, and the push reports
/// `CapacityExhausted`.
#[test]
fn array_growth_stops_at_the_byte_ceiling_without_corruption() {
    let mut heap = Heap::new(64);
    heap.set_max_heap_bytes(1_024);
    let element = nexa_bytecode::ValueType::I32;
    let array = heap
        .allocate_array(nexa_bytecode::array_type(element), element)
        .expect("empty array");
    let mut accepted = 0_i32;
    let mut refused = None;
    for index in 0..100 {
        match heap.array_push(array, RuntimeValue::I32(index)) {
            Ok(()) => accepted += 1,
            Err(error) => {
                refused = Some(error);
                break;
            }
        }
    }
    assert_eq!(
        refused,
        Some(nexa_runtime::HeapError::CapacityExhausted),
        "growth must hit the byte ceiling before 100 pushes"
    );
    let length = heap.array_len(array).expect("array stays valid");
    assert_eq!(
        i32::try_from(length).expect("bounded"),
        accepted,
        "every accepted push is visible, the refused one changed nothing"
    );
    let values = heap.array_values(array).expect("contents stay readable");
    assert_eq!(values.get(0), Some(RuntimeValue::I32(0)));
    assert_eq!(
        values.get(usize::try_from(accepted - 1).expect("bounded")),
        Some(RuntimeValue::I32(accepted - 1))
    );
}

/// G6 gate: the realm forwards `RealmConfig::max_heap_bytes` to the heap.
#[test]
fn realm_config_wires_the_heap_byte_ceiling() {
    use nexa_runtime::{Object, RealmConfig, RealmRuntime};
    let config = RealmConfig {
        max_heap_bytes: 8,
        ..RealmConfig::default()
    };
    let mut realm = RealmRuntime::isolated(config);
    let refused = realm.allocate(Object::String(String::from("longer-than-eight-bytes")));
    assert!(
        refused.is_err(),
        "a realm-configured byte ceiling refuses oversized payloads"
    );
    let mut unlimited = RealmRuntime::isolated(RealmConfig::default());
    unlimited
        .allocate(Object::String(String::from("longer-than-eight-bytes")))
        .expect("the default configuration stays unlimited");
}
