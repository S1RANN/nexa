#[test]
fn resource_token() {
    let (mut realm, module, _, _) =
        super::support::realm_with([nexa_bytecode::Instruction::Return { source: 0 }]);
    let (scope, task) = super::support::spawn(&mut realm, module);
    let token = realm
        .create_resource_token(task, nexa_runtime::RuntimeHostDomain::Render)
        .unwrap();
    let result = realm.poll_task_raw(task, 16).unwrap();
    let report = realm
        .tick(nexa_runtime::TickBudget {
            max_tasks: 0,
            frame_fuel_budget: 0,
            collect_garbage: false,
        })
        .unwrap();
    let extra = format!("token={token:?}\ntick={report:?}\n");
    super::support::assert_snapshot(
        "resource_token",
        &super::support::snapshot(&realm, scope, task, &result, &extra),
        include_str!("../snapshots/runtime/resource_token.snap"),
    );
}
