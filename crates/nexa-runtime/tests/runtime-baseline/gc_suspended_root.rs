#[test]
fn gc_suspended_root() {
    use nexa_bytecode::{FunctionBuilder, FunctionEffect, Instruction, Signature, ValueType};
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::Ref],
            result: Some(ValueType::Ref),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .set_root(0)
        .unwrap()
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let verified = super::support::verified(vec![function.finish().unwrap()]);
    let (host, schema) = super::support::hashes();
    let mut realm = nexa_runtime::RealmRuntime::isolated(nexa_runtime::RealmConfig::default());
    let module = realm.load_module(verified, host, schema).unwrap();
    let scope = realm.create_scope(None).unwrap();
    let reference = realm
        .allocate(nexa_runtime::Object::String("root".into()))
        .unwrap();
    let task = realm
        .spawn_task(
            module,
            0,
            &[nexa_runtime::RuntimeValue::Ref(reference)],
            super::support::task_config(scope),
        )
        .unwrap();
    let result = realm.poll_task(task, 16).unwrap();
    let live = realm.collect_garbage().unwrap();
    realm.cancel_scope(scope).unwrap();
    let reclaimed = realm.collect_garbage().unwrap();
    let extra = format!("live={live:?}\nreclaimed={reclaimed:?}\n");
    super::support::assert_snapshot(
        "gc_suspended_root",
        &super::support::snapshot(&realm, scope, task, &result, &extra),
        include_str!("../snapshots/runtime/gc_suspended_root.snap"),
    );
}
