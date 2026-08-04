use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, ScriptExport, Signature,
    StateSchema, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{RealmConfig, RealmRuntime, RestartReloadOutcome, RestartReloadPolicy};
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

fn partially_changed_module(value: i32) -> VerifiedModule {
    let mut shared = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(ValueType::I32),
        },
        1,
    );
    shared
        .emit(Instruction::LoadI32 { dst: 0, value: 41 })
        .emit(Instruction::Return { source: 0 });

    let mut changed = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(ValueType::I32),
        },
        1,
    );
    changed
        .effect(FunctionEffect::Task)
        .emit(Instruction::LoadI32 { dst: 0, value })
        .emit(Instruction::Return { source: 0 });

    let mut module = ModuleBuilder::new();
    let schema = StateSchema::default().fingerprint();
    module.metadata(HOST, schema);
    module.function(shared.finish().expect("shared cache fixture function"));
    let changed = module.function(changed.finish().expect("changed cache fixture function"));
    module.script_export(ScriptExport {
        stable_id: EXPORT,
        function: changed,
        signature: Signature {
            parameters: Vec::new(),
            result: Some(ValueType::I32),
        },
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("verified partial cache fixture")
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
    let first = realm.execution_image_cache_inspection();
    assert_eq!(
        (
            first.entries,
            first.capacity,
            first.hits,
            first.misses,
            first.layout_reuses,
            first.module_abi_reuses,
            first.profile_metadata_reuses,
            first.function_reuses,
            first.string_pool_reuses,
        ),
        (1, 3, 0, 1, 0, 0, 0, 0, 0)
    );
    assert!(first.logical_executable_payload_bytes > 0);
    assert_eq!(
        first.logical_executable_payload_bytes,
        first.unique_executable_payload_bytes
    );
    assert_eq!(first.shared_executable_payload_bytes, 0);

    realm
        .load_module(identical, HOST, schema)
        .expect("identical image admission");
    let identical = realm.execution_image_cache_inspection();
    assert_eq!(
        (
            identical.entries,
            identical.capacity,
            identical.hits,
            identical.misses,
            identical.layout_reuses,
            identical.module_abi_reuses,
            identical.profile_metadata_reuses,
            identical.function_reuses,
            identical.string_pool_reuses,
        ),
        (1, 3, 1, 1, 1, 1, 0, 1, 1)
    );
    assert_eq!(
        identical.logical_executable_payload_bytes,
        identical.unique_executable_payload_bytes
    );

    realm
        .load_module(distinct, HOST, schema)
        .expect("distinct image admission");
    let distinct = realm.execution_image_cache_inspection();
    assert_eq!(
        (
            distinct.entries,
            distinct.capacity,
            distinct.hits,
            distinct.misses,
            distinct.layout_reuses,
            distinct.module_abi_reuses,
            distinct.profile_metadata_reuses,
            distinct.function_reuses,
            distinct.string_pool_reuses,
        ),
        (2, 3, 1, 2, 2, 2, 0, 1, 2)
    );
    assert!(distinct.shared_executable_payload_bytes > 0);
    assert_eq!(
        distinct.logical_executable_payload_bytes,
        distinct
            .unique_executable_payload_bytes
            .saturating_add(distinct.shared_executable_payload_bytes)
    );
}

#[test]
fn reload_reuses_content_identical_assets_but_keeps_epoch_publication_independent() {
    let schema = StateSchema::default().fingerprint();
    let config = RealmConfig {
        max_modules: 3,
        execution_image_cache_capacity: 3,
        ..RealmConfig::default()
    };
    let mut realm = RealmRuntime::isolated(config);
    let old = realm
        .load_module(partially_changed_module(7), HOST, schema)
        .expect("old partial image");
    let old_epoch = realm.active_module_epoch(old).expect("old epoch");

    let outcome = realm
        .restart_reload(
            old,
            partially_changed_module(9),
            RestartReloadPolicy::default(),
        )
        .expect("partial reload");
    let RestartReloadOutcome::Committed(candidate) = outcome else {
        panic!("partial reload must commit, got {outcome:?}");
    };
    assert_ne!(candidate, old, "candidate keeps an independent module slot");
    assert!(
        realm
            .active_module_epoch(candidate)
            .expect("candidate epoch")
            > old_epoch,
        "candidate keeps an independent epoch"
    );
    assert_eq!(realm.active_root(), Some(candidate));
    let cache = realm.execution_image_cache_inspection();
    assert_eq!(
        (
            cache.entries,
            cache.capacity,
            cache.hits,
            cache.misses,
            cache.layout_reuses,
            cache.module_abi_reuses,
            cache.profile_metadata_reuses,
            cache.function_reuses,
            cache.string_pool_reuses,
        ),
        (2, 3, 0, 2, 1, 1, 0, 1, 1)
    );
    assert!(cache.shared_executable_payload_bytes > 0);
    assert_eq!(
        realm.host_import_plan_cache_inspection(),
        nexa_runtime::HostImportPlanCacheInspection {
            entries: 1,
            capacity: 3,
            hits: 1,
            misses: 1,
        }
    );
}
