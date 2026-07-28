use nexa_core::{machine_event_id, machine_invariant_hash, machine_state_id};
use nexa_machine::MachineSpec;
use nexa_model::ReferenceMachine;
use nexa_runtime::{RuntimeError, RuntimeLimits, TaskError, TaskRuntime, TaskState};

type TaskOperation = fn(&mut TaskRuntime, nexa_runtime::TaskHandle) -> Result<(), RuntimeError>;

#[test]
fn runtime_task_trace_matches_reference_model_for_acceptance_path() {
    let spec = MachineSpec::parse(include_str!("../../../specs/machines/task.machine.spec"))
        .expect("task spec is valid");
    let mut model = ReferenceMachine::new(&spec);
    let mut runtime = TaskRuntime::new(17, RuntimeLimits::default());
    let owner = runtime.create_scope(None).unwrap();
    let module_epoch = 23;

    let mut expected = Vec::new();
    let admission = model.apply("Admit", |_| true).unwrap();
    expected.push(expected_trace(&admission, owner.raw()));
    let task = runtime.admit_task(owner, module_epoch, true).unwrap();

    let trace_len = runtime.trace().records().len();
    assert!(matches!(
        runtime.yield_fuel_task(task),
        Err(RuntimeError::Task(TaskError::Transition(_)))
    ));
    assert_eq!(runtime.trace().records().len(), trace_len + 1);
    let rejected = runtime.trace().records().last().unwrap();
    assert_eq!(
        rejected.disposition,
        nexa_core::TransitionDisposition::Undefined
    );
    assert_eq!(rejected.old_state, rejected.new_state);
    assert!(rejected.resource_deltas.is_empty());
    assert_eq!(runtime.task_snapshot(task).unwrap().state, TaskState::Ready);

    let events: &[(&str, TaskOperation)] = &[
        ("Poll", TaskRuntime::poll_task),
        ("YieldFuel", TaskRuntime::yield_fuel_task),
        ("Resume", TaskRuntime::resume_fuel_task),
        ("RequestCancel", TaskRuntime::request_task_cancel),
        ("ReachSafepoint", TaskRuntime::reach_task_safepoint),
        ("Clean", TaskRuntime::finish_cancel_without_cleanup),
    ];
    for (name, operation) in events {
        let step = model.apply(name, |_| true).unwrap();
        expected.push(expected_trace(&step, owner.raw()));
        operation(&mut runtime, task).unwrap();
    }

    let actual = runtime
        .trace()
        .records()
        .iter()
        .filter(|record| {
            record.machine_kind == nexa_core::MachineKind::Task
                && record.disposition == nexa_core::TransitionDisposition::Applied
        })
        .collect::<Vec<_>>();
    assert_eq!(actual.len(), expected.len());
    for (record, expected) in actual.iter().zip(&expected) {
        assert_eq!(record.transition_id, expected.transition_id);
        assert_eq!(record.old_state, expected.old_state);
        assert_eq!(record.event, expected.event);
        assert_eq!(record.new_state, expected.new_state);
        assert_eq!(record.resource_deltas, expected.resource_deltas);
        assert_eq!(record.invariant_hash, expected.invariant_hash);
        assert_eq!(record.owner_scope, Some(owner.raw()));
        assert_eq!(record.module_epoch, module_epoch);
    }

    assert_eq!(model.state(), "Cancelled");
    assert_eq!(model.resources()["task_slot"], 0);
    assert!(runtime.task_snapshot(task).is_err());
    let owner_snapshot = runtime.scope_snapshot(owner).unwrap();
    assert_eq!(owner_snapshot.transient_children, 0);
    assert_eq!(owner_snapshot.persistent_children, 0);
    assert!(
        runtime
            .trace()
            .records()
            .iter()
            .zip(runtime.trace().records().iter().skip(1))
            .all(|(previous, current)| previous.sequence + 1 == current.sequence)
    );
    assert!(
        runtime
            .trace()
            .records()
            .iter()
            .any(|record| record.machine_kind == nexa_core::MachineKind::Scope)
    );
}

struct ExpectedTrace {
    transition_id: nexa_core::StableId,
    old_state: nexa_core::StableId,
    event: nexa_core::StableId,
    new_state: nexa_core::StableId,
    resource_deltas: nexa_core::InlineDeltas,
    invariant_hash: u64,
}

fn expected_trace(step: &nexa_model::ReferenceStep, owner: nexa_core::RawHandle) -> ExpectedTrace {
    let resources = step
        .resources
        .iter()
        .map(|(name, amount)| (name.as_str(), *amount))
        .collect::<Vec<_>>();
    ExpectedTrace {
        transition_id: step.transition_id,
        old_state: machine_state_id("Task", &step.old_state),
        event: machine_event_id("Task", &step.event),
        new_state: machine_state_id("Task", &step.new_state),
        resource_deltas: {
            let mut deltas = nexa_core::InlineDeltas::new();
            for delta in &step.resource_deltas {
                deltas
                    .try_push(nexa_core::ResourceDelta {
                        resource: nexa_core::StableId::from_name(&delta.resource),
                        amount: delta.amount,
                    })
                    .unwrap();
            }
            deltas
        },
        invariant_hash: machine_invariant_hash("Task", &step.new_state, Some(owner), &resources),
    }
}
