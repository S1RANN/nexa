use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, ScriptExport, Signature,
    StateSchema, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{RealmConfig, RealmRuntime};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST: StableId = StableId(0x4558_4543_494d_4147);
const EXPORT: StableId = StableId(0x4558_4543_4558_504f);

fn module(value: i32) -> VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(ValueType::I32),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::LoadI32 { dst: 0, value })
        .emit(Instruction::Return { source: 0 });
    let function = function.finish().expect("cache fixture function");
    let mut module = ModuleBuilder::new();
    let schema = StateSchema::default().fingerprint();
    module.metadata(HOST, schema);
    let function = module.function(function);
    module.script_export(ScriptExport {
        stable_id: EXPORT,
        function,
        signature: Signature {
            parameters: Vec::new(),
            result: Some(ValueType::I32),
        },
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("verified cache fixture")
}

#[test]
fn identical_portable_artifacts_reuse_one_bounded_execution_image() {
    let schema = StateSchema::default().fingerprint();
    let first = module(7);
    let identical = first.clone();
    let distinct = module(9);
    let config = RealmConfig {
        max_modules: 3,
        execution_image_cache_capacity: 3,
        ..RealmConfig::default()
    };
    let mut realm = RealmRuntime::isolated(config);

    realm
        .load_module(first, HOST, schema)
        .expect("first image admission");
    assert_eq!(
        realm.execution_image_cache_inspection(),
        nexa_runtime::ExecutionImageCacheInspection {
            entries: 1,
            capacity: 3,
            hits: 0,
            misses: 1,
        }
    );

    realm
        .load_module(identical, HOST, schema)
        .expect("identical image admission");
    assert_eq!(
        realm.execution_image_cache_inspection(),
        nexa_runtime::ExecutionImageCacheInspection {
            entries: 1,
            capacity: 3,
            hits: 1,
            misses: 1,
        }
    );

    realm
        .load_module(distinct, HOST, schema)
        .expect("distinct image admission");
    assert_eq!(
        realm.execution_image_cache_inspection(),
        nexa_runtime::ExecutionImageCacheInspection {
            entries: 2,
            capacity: 3,
            hits: 1,
            misses: 2,
        }
    );
}
