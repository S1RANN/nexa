#[test]
fn reload_activation_fault() {
    use nexa_bytecode::{FunctionBuilder, FunctionEffect, Instruction, Signature, ValueType};
    let build = || {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        function
            .effect(FunctionEffect::Task)
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 0 });
        super::support::verified(vec![function.finish().unwrap()])
    };
    let build_candidate = || {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::Return { source: 0 });
        let mut task = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        task.effect(FunctionEffect::Task)
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 0 });
        super::support::verified(vec![migration.finish().unwrap(), task.finish().unwrap()])
    };
    let (host, schema) = super::support::hashes();
    let mut realm = nexa_runtime::RealmRuntime::new(nexa_runtime::RealmConfig::default());
    let old = realm.load_module(build(), host, schema).unwrap();
    let (scope, task) = super::support::spawn(&mut realm, old);
    let candidate = realm
        .prepare_reload(old, build_candidate(), host, schema)
        .unwrap();
    realm.quiesce_reload().unwrap();
    realm
        .stage_reload(0, &[nexa_runtime::RuntimeValue::I32(7)])
        .unwrap();
    let activation = realm.commit_reload(|_| Err("activation".into()));
    let rejected = realm.call(
        candidate,
        0,
        &[nexa_runtime::RuntimeValue::I32(1)],
        super::support::task_config(scope),
    );
    let result = realm.poll_task(task, 32).unwrap();
    let extra = format!("activation={activation:?}\nrejected={rejected:?}\n");
    super::support::assert_snapshot(
        "reload_activation_fault",
        &super::support::snapshot(&realm, scope, task, &result, &extra),
        include_str!("../snapshots/runtime/reload_activation_fault.snap"),
    );
}
