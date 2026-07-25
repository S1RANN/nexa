#[test]
fn host_request() {
    let (mut realm, module, _, _) =
        super::support::realm_with([nexa_bytecode::Instruction::Return { source: 0 }]);
    let (scope, task) = super::support::spawn(&mut realm, module);
    let mut pending = realm.create_host_request(task).unwrap();
    realm.wait_for_request(task, pending.request).unwrap();
    assert_eq!(
        realm.poll_task(task, 16).unwrap(),
        nexa_runtime::PollResult::Pending(nexa_runtime::PendingReason::HostRequest)
    );
    pending
        .ticket
        .complete(nexa_runtime::HostPayload::I32(9))
        .unwrap();
    let report = realm
        .tick(nexa_runtime::TickBudget {
            max_tasks: 1,
            frame_fuel_budget: 16,
            collect_garbage: false,
        })
        .unwrap();
    let result = nexa_runtime::PollResult::Completed(Some(nexa_runtime::RuntimeValue::I32(7)));
    let extra = format!(
        "tick={report:?}\ndiscarded={}\n",
        realm.discarded_late_host_results()
    );
    super::support::assert_snapshot(
        "host_request",
        &super::support::snapshot(&realm, scope, task, &result, &extra),
        include_str!("../snapshots/runtime/host_request.snap"),
    );
}
