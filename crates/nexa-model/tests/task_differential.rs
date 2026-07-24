use nexa_core::{machine_event_id, machine_invariant_hash, machine_state_id};
use nexa_machine::MachineSpec;
use nexa_model::ReferenceMachine;
use nexa_runtime::{ScopeManager, TaskError, TaskEvent, TaskManager, TaskState};

#[test]
fn runtime_task_trace_matches_reference_model_for_acceptance_path() {
    let spec = MachineSpec::parse(include_str!("../../../specs/machines/task.machine.spec"))
        .expect("task spec is valid");
    let mut model = ReferenceMachine::new(&spec);
    let mut scopes = ScopeManager::new(17);
    let owner = scopes.create(None).unwrap();
    let mut runtime = TaskManager::new(17);
    let module_epoch = 23;

    let mut expected = Vec::new();
    let admission = model.apply("Admit", |_| true).unwrap();
    expected.push(expected_trace(&admission, owner.raw()));
    let task = runtime
        .admit(&mut scopes, owner, module_epoch, true)
        .unwrap();

    let trace_len = runtime.trace().records().len();
    assert!(matches!(
        runtime.apply(&mut scopes, task, TaskEvent::YieldFuel),
        Err(TaskError::Transition(_))
    ));
    assert_eq!(runtime.trace().records().len(), trace_len);
    assert_eq!(runtime.snapshot(task).unwrap().state, TaskState::Ready);

    let events = [
        ("Poll", TaskEvent::Poll),
        ("YieldFuel", TaskEvent::YieldFuel),
        ("Resume", TaskEvent::Resume),
        ("RequestReloadPause", TaskEvent::RequestReloadPause),
        ("ReachSafepoint", TaskEvent::ReachSafepoint),
        ("RollbackReload", TaskEvent::RollbackReload),
        ("RequestCancel", TaskEvent::RequestCancel),
        ("ReachSafepoint", TaskEvent::ReachSafepoint),
        ("Clean", TaskEvent::Clean),
    ];
    for (name, event) in events {
        let step = model.apply(name, |_| true).unwrap();
        expected.push(expected_trace(&step, owner.raw()));
        runtime.apply(&mut scopes, task, event).unwrap();
    }

    let actual = runtime.trace().records();
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
    assert_eq!(runtime.active_len(), 0);
    let owner_snapshot = scopes.snapshot(owner).unwrap();
    assert_eq!(owner_snapshot.transient_children, 0);
    assert_eq!(owner_snapshot.persistent_children, 0);
}

struct ExpectedTrace {
    transition_id: nexa_core::StableId,
    old_state: nexa_core::StableId,
    event: nexa_core::StableId,
    new_state: nexa_core::StableId,
    resource_deltas: Vec<nexa_core::ResourceDelta>,
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
        resource_deltas: step
            .resource_deltas
            .iter()
            .map(|delta| nexa_core::ResourceDelta {
                resource: delta.resource.clone(),
                amount: delta.amount,
            })
            .collect(),
        invariant_hash: machine_invariant_hash("Task", &step.new_state, Some(owner), &resources),
    }
}
