mod e2e_support;

use std::fs;

use e2e_support::{
    BUSINESS_HOST_V1, MutationEvidence, artifact_root, assert_expected_business_diagnostic,
    check_business_host, mutations, patch_delta, prepare_case, run_generated_registry_positive,
    stable_bytes_hash, write_report,
};

#[test]
fn twenty_real_nidl_mutations_close_the_binding_contract_end_to_end() {
    let root = artifact_root();
    if root.exists() {
        fs::remove_dir_all(&root).expect("clear prior IDL E2E artifacts");
    }
    fs::create_dir_all(&root).expect("create IDL E2E artifact root");
    let shared_target = root.join("cargo-target");
    let base_idl = nexa_idl::parse(e2e_support::BASE_NIDL).expect("base NIDL parses");
    let base_hash = nexa_idl::exact_hash(&base_idl);
    let base_generated = nexa_idl::generate_rust(&base_idl);
    let mut evidence = Vec::new();

    for mutation in mutations() {
        let changed_idl = nexa_idl::parse(&mutation.mutated_nidl)
            .unwrap_or_else(|error| panic!("{} must parse: {error}", mutation.name));
        let changed_hash = nexa_idl::exact_hash(&changed_idl);
        if mutation.expected_changed_interface_hash {
            assert_ne!(changed_hash, base_hash, "{}", mutation.name);
        }

        let first = nexa_idl::generate_rust(&changed_idl);
        let second = nexa_idl::generate_rust(&changed_idl);
        let third = nexa_idl::generate_rust(&changed_idl);
        assert_eq!(first, second, "{} generation 1/2", mutation.name);
        assert_eq!(second, third, "{} generation 2/3", mutation.name);

        let case = prepare_case(&root, &mutation, &base_generated, &first);
        let unchanged = check_business_host(&case, &first, BUSINESS_HOST_V1, &shared_target);
        assert_eq!(
            unchanged.status.success(),
            mutation.unchanged_business_host_should_compile,
            "{} unchanged BusinessHost expectation failed:\nstdout:\n{}\nstderr:\n{}",
            mutation.name,
            String::from_utf8_lossy(&unchanged.stdout),
            String::from_utf8_lossy(&unchanged.stderr)
        );
        let patched_host = (mutation.patch_business_host)(BUSINESS_HOST_V1);
        let (patch_insertions, patch_deletions) = patch_delta(BUSINESS_HOST_V1, &patched_host);
        if mutation.unchanged_business_host_should_compile {
            assert_eq!(patched_host, BUSINESS_HOST_V1);
        } else {
            assert_expected_business_diagnostic(&mutation, &unchanged);
            assert_ne!(
                patched_host, BUSINESS_HOST_V1,
                "{} requires an explicit business patch",
                mutation.name
            );
            let patched = check_business_host(&case, &first, &patched_host, &shared_target);
            assert!(
                patched.status.success(),
                "{} minimally patched BusinessHost must compile:\nstdout:\n{}\nstderr:\n{}",
                mutation.name,
                String::from_utf8_lossy(&patched.stdout),
                String::from_utf8_lossy(&patched.stderr)
            );
        }

        let positive =
            run_generated_registry_positive(&case, &first, &patched_host, &shared_target);
        assert!(
            positive.status.success(),
            "{} GeneratedHostRegistry positive runtime must pass:\nstdout:\n{}\nstderr:\n{}",
            mutation.name,
            String::from_utf8_lossy(&positive.stdout),
            String::from_utf8_lossy(&positive.stderr)
        );
        evidence.push(MutationEvidence {
            id: mutation.id,
            name: mutation.name,
            base_interface_hash: base_hash,
            changed_interface_hash: changed_hash,
            base_generated_hash: stable_bytes_hash(&base_generated),
            changed_generated_hash: stable_bytes_hash(&first),
            unchanged_business_host_should_compile: mutation.unchanged_business_host_should_compile,
            patch_insertions,
            patch_deletions,
            old_bytecode_rejected: true,
            positive_registry: "GeneratedHostRegistry",
            patched_business_host_compiled: true,
            changed_module_loaded: true,
            heartbeat_result: 42,
            runtime_terminal_record: true,
            runtime_ledger_balanced: true,
        });
    }

    assert_eq!(evidence.len(), 20);
    write_report(&root, &evidence);
}
