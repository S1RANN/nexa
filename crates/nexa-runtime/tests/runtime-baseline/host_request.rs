#[test]
fn host_request() {
    let (mut realm, module, _, _) =
        super::support::realm_with([nexa_bytecode::Instruction::Return { source: 0 }]);
    let (scope, task) = super::support::spawn(&mut realm, module);
    let request = realm.create_host_request(task).unwrap();
    realm.wait_for_request(task, request).unwrap();
    assert_eq!(
        realm.poll_task(task, 16).unwrap(),
        nexa_runtime::PollResult::Pending(nexa_runtime::PendingReason::HostRequest)
    );
    realm
        .completion_sender()
        .complete(nexa_runtime::HostCompletion {
            realm_id: realm.realm_id(),
            module_id: module.raw().index,
            epoch: realm.module_epoch(module).unwrap(),
            request,
            payload: nexa_runtime::HostPayload::I32(9),
        })
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
