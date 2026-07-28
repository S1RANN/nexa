#[test]
fn fast_complete() {
    let (mut realm, module, _, _) =
        super::support::realm_with([nexa_bytecode::Instruction::Return { source: 0 }]);
    let (scope, task) = super::support::spawn(&mut realm, module);
    let result = realm.poll_task_raw(task, 16).unwrap();
    super::support::assert_snapshot(
        "fast_complete",
        &super::support::snapshot(&realm, scope, task, &result, ""),
        include_str!("../snapshots/runtime/fast_complete.snap"),
    );
}
