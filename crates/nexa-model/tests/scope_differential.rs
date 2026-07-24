use nexa_machine::MachineSpec;
use nexa_runtime::ScopeManager;

#[test]
fn runtime_scope_trace_matches_normative_transition_ids() {
    let spec = MachineSpec::parse(include_str!("../../../specs/machines/scope.machine.spec"))
        .expect("scope spec is valid");
    let expected_names = [
        "SCOPE_CREATED_ACTIVATE_ACTIVE",
        "SCOPE_ACTIVE_CANCEL_REQUESTED",
        "SCOPE_CANCEL_REQUESTED_CHILDREN_OBSERVED_CANCELLING",
        "SCOPE_CANCELLING_CHILDREN_FINISHED",
        "SCOPE_CANCELLED_DESTROY_DESTROYED",
    ];
    let expected = expected_names
        .iter()
        .map(|name| {
            let transition = spec
                .transitions
                .iter()
                .find(|transition| transition.name == *name)
                .expect("named transition exists");
            spec.transition_id(transition)
        })
        .collect::<Vec<_>>();

    let mut runtime = ScopeManager::new(11);
    let scope = runtime.create(None).unwrap();
    runtime.request_cancel(scope).unwrap();
    runtime.begin_cancelling(scope).unwrap();
    runtime.finish_cancelling(scope).unwrap();
    runtime.destroy(scope).unwrap();

    let actual = runtime
        .trace()
        .records()
        .iter()
        .map(|record| record.transition_id)
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}
