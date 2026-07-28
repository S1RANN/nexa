mod e2e_support;

use std::fs;

use e2e_support::{
    MutationEvidence, artifact_root, assert_pre_interpreter_rejection, check_host,
    generated_host_impl, mutations, prepare_case, stable_bytes_hash, write_report,
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
    let unchanged_host = generated_host_impl(&base_generated, "UnchangedHost");
    let mut evidence = Vec::new();

    for mutation in mutations() {
        let changed_idl = nexa_idl::parse(&mutation.source)
            .unwrap_or_else(|error| panic!("{} must parse: {error}", mutation.name));
        let changed_hash = nexa_idl::exact_hash(&changed_idl);
        assert_ne!(changed_hash, base_hash, "{}", mutation.name);

        let first = nexa_idl::generate_rust(&changed_idl);
        let second = nexa_idl::generate_rust(&changed_idl);
        let third = nexa_idl::generate_rust(&changed_idl);
        assert_eq!(first, second, "{} generation 1/2", mutation.name);
        assert_eq!(second, third, "{} generation 2/3", mutation.name);

        let case = prepare_case(&root, &mutation, &base_generated, &first);
        let unchanged = check_host(&case, &first, &unchanged_host, &shared_target);
        assert_eq!(
            unchanged.status.success(),
            mutation.unchanged_host_should_compile,
            "{} unchanged Host expectation failed:\n{}",
            mutation.name,
            String::from_utf8_lossy(&unchanged.stderr)
        );
        if !mutation.unchanged_host_should_compile {
            let stderr = String::from_utf8_lossy(&unchanged.stderr);
            assert!(
                stderr.contains("trait")
                    || stderr.contains("method")
                    || stderr.contains("unresolved")
                    || stderr.contains("cannot find"),
                "{} failed for an unrelated reason:\n{stderr}",
                mutation.name
            );
            let patched_host = generated_host_impl(&first, "PatchedHost");
            let patched = check_host(&case, &first, &patched_host, &shared_target);
            assert!(
                patched.status.success(),
                "{} minimally patched Host must compile:\n{}",
                mutation.name,
                String::from_utf8_lossy(&patched.stderr)
            );
        }

        assert_pre_interpreter_rejection(&base_idl, changed_hash);
        evidence.push(MutationEvidence {
            id: mutation.id,
            name: mutation.name,
            base_interface_hash: base_hash,
            changed_interface_hash: changed_hash,
            base_generated_hash: stable_bytes_hash(&base_generated),
            changed_generated_hash: stable_bytes_hash(&first),
            unchanged_host_should_compile: mutation.unchanged_host_should_compile,
        });
    }

    assert_eq!(evidence.len(), 20);
    write_report(&root, &evidence);
}
