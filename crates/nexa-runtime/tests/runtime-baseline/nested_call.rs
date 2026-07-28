#[test]
fn nested_call() {
    use nexa_bytecode::{FunctionBuilder, FunctionEffect, Instruction, Signature, ValueType};
    let mut caller = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        2,
    );
    caller.effect(FunctionEffect::Task).emit(Instruction::Call {
        function: 1,
        args_base: 0,
        args_count: 1,
        dst: 1,
    });
    caller.emit(Instruction::Return { source: 1 });
    let mut identity_function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    identity_function.emit(Instruction::Return { source: 0 });
    let verified = super::support::verified(vec![
        caller.finish().unwrap(),
        identity_function.finish().unwrap(),
    ]);
    let (host, schema) = super::support::hashes();
    let mut realm = nexa_runtime::RealmRuntime::isolated(nexa_runtime::RealmConfig::default());
    let module = realm.load_module(verified, host, schema).unwrap();
    let (scope, task) = super::support::spawn(&mut realm, module);
    let result = realm.poll_task_raw(task, 16).unwrap();
    super::support::assert_snapshot(
        "nested_call",
        &super::support::snapshot(&realm, scope, task, &result, ""),
        include_str!("../snapshots/runtime/nested_call.snap"),
    );
}
