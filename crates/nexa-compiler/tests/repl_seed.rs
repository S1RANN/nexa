use nexa_analysis::{
    REPL_ENVIRONMENT_STATE_VERSION, REPL_ENVIRONMENT_TYPE_NAME, REPL_MODULE_PATH, REPL_PACKAGE_ID,
    repl_environment_symbol,
};
use nexa_compiler::compile_typed_repl_seed;
use nexa_core::StableId;
use nexa_verifier::{VerifierLimits, verify};

const CONSOLE_CONTRACT_ID: StableId = StableId(0x434f_4e53_4f4c_4501);

#[test]
fn revision_zero_repl_seed_is_the_canonical_reserved_environment() {
    let output =
        compile_typed_repl_seed(CONSOLE_CONTRACT_ID).expect("canonical REPL seed compiles");
    let package = &output.package;
    let module = &package.module;
    let [state] = module.state_schema.types.as_slice() else {
        panic!("seed must contain exactly one state type");
    };

    assert_eq!(module.host_contract_id, Some(CONSOLE_CONTRACT_ID));
    assert_eq!(state.stable_id, repl_environment_symbol().0);
    assert_eq!(state.version, REPL_ENVIRONMENT_STATE_VERSION);
    assert!(state.fields.is_empty());
    assert!(module.functions.is_empty());
    assert!(module.host_imports.is_empty());
    assert!(module.exports.is_empty());
    assert_eq!(module.reload_metadata.migration_entry, None);
    assert_eq!(module.reload_metadata.activation_entry, None);
    assert_eq!(
        output.state_schema_fingerprint,
        module.state_schema.fingerprint()
    );
    assert_eq!(
        module.state_schema_fingerprint,
        output.state_schema_fingerprint
    );
    assert_eq!(
        module.reload_metadata.state_schema_fingerprint,
        output.state_schema_fingerprint
    );
    assert_eq!(
        package.state_schema_fingerprint,
        Some(output.state_schema_fingerprint)
    );

    let [state_surface] = package.state_surface.as_slice() else {
        panic!("seed must retain one state surface");
    };
    assert_eq!(state_surface.package_id, REPL_PACKAGE_ID);
    assert_eq!(state_surface.module_path, REPL_MODULE_PATH);
    assert_eq!(state_surface.name, REPL_ENVIRONMENT_TYPE_NAME);
    assert_eq!(state_surface.stable_id, repl_environment_symbol());
    assert_eq!(state_surface.version, REPL_ENVIRONMENT_STATE_VERSION);
    assert!(state_surface.fields.is_empty());
    assert_eq!(package.sources.len(), 1);
    assert_eq!(package.debug_info.root_package_id, REPL_PACKAGE_ID);
    assert_eq!(package.debug_info.entry_module, REPL_MODULE_PATH);
    assert!(package.debug_info.functions.is_empty());

    verify(module.clone(), VerifierLimits::default()).expect("revision-zero seed verifies");
}
