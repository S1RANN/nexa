#[test]
fn scope_cancel() {
    let (mut realm, module, _, _) = super::support::realm_with([
        nexa_bytecode::Instruction::Yield,
        nexa_bytecode::Instruction::Return { source: 0 },
    ]);
    let (scope, task) = super::support::spawn_test_task(&mut realm, module);
    let first = realm.poll_task(task, 16).unwrap();
    realm.cancel_scope(scope).unwrap();
    let result = nexa_runtime::TaskPoll::Cancelled(nexa_runtime::CancelReason::ScopeCancelled);
    let extra = format!("first={first:?}\n");
    super::support::assert_snapshot(
        "scope_cancel",
        &super::support::snapshot(&realm, scope, task, &result, &extra),
        include_str!("../snapshots/runtime/scope_cancel.snap"),
    );
}
