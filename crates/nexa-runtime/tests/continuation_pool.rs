//! H1 gate: realm task lifecycles recycle continuation arena storage.
//! Terminal polls return the arena to a bounded pool and the next
//! admission reuses it, so steady-state spawn/complete churn performs no
//! continuation storage allocation (quantified end to end by the
//! benchmark's `async_admission` case: 3 allocations/op down to 0).

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, ScriptExport, Signature,
    StateSchema, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{RealmConfig, RealmRuntime, RuntimeValue, StepConfig, TaskLimits, TaskPoll};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST: StableId = StableId(0x504f_4f4c_4841_5348);
const EXPORT: StableId = StableId(0x504f_4f4c_5441_534b);

fn task_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut function = FunctionBuilder::new(signature.clone(), 1);
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, StateSchema::default().fingerprint());
    let function = module.function(function.finish().expect("task function"));
    module.script_export(ScriptExport {
        stable_id: EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("pool gate module")
}

#[test]
fn terminal_tasks_feed_the_pool_and_admission_drains_it() {
    let verified = task_module();
    let schema = verified.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm.load_module(verified, HOST, schema).expect("load");
    let scope = realm.create_scope(None).expect("scope");
    let config = StepConfig {
        owner: scope,
        priority: 1,
        fuel_slice: 64,
        cumulative_budget: 1_024,
        limits: TaskLimits::default(),
    };
    assert_eq!(realm.continuation_pool_depth(), 0);
    for round in 0..8 {
        let task = realm
            .spawn_task(module, EXPORT, &[RuntimeValue::I32(round)], config)
            .expect("spawn");
        // Reuse: after the first completion, every admission drains the
        // pooled arena instead of reserving fresh storage.
        assert_eq!(
            realm.continuation_pool_depth(),
            0,
            "admission consumes the pooled arena (round {round})"
        );
        let poll = realm.poll_task(task, 64).expect("poll to completion");
        assert!(
            matches!(poll, TaskPoll::Completed(RuntimeValue::I32(value)) if value == round),
            "task completes with its argument (round {round})"
        );
        assert_eq!(
            realm.continuation_pool_depth(),
            1,
            "the terminal poll returns the arena to the pool (round {round})"
        );
    }
}
