#[test]
fn fuel_yield() {
    let (mut realm, module, _, _) =
        super::support::realm_with([nexa_bytecode::Instruction::Return { source: 0 }]);
    let (scope, task) = super::support::spawn(&mut realm, module);
    let first = realm.poll_task(task, 0).unwrap();
    let second = realm.poll_task(task, 16).unwrap();
    let extra = format!("first={first:?}\n");
    super::support::assert_snapshot(
        "fuel_yield",
        &super::support::snapshot(&realm, scope, task, &second, &extra),
        include_str!("../snapshots/runtime/fuel_yield.snap"),
    );
}
