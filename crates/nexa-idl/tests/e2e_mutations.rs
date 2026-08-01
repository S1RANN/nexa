mod e2e_support;

use std::fs;
use std::path::Path;

use e2e_support::{
    BUSINESS_HOST_V1, MutationCase, MutationEvidence, RuntimeMutationEvidence, artifact_root,
    assert_expected_business_diagnostic, check_business_host, mutations, patch_delta, prepare_case,
    read_runtime_evidence, run_generated_registry_positive, stable_bytes_hash, write_report,
};
use nexa_core::StableId;

#[test]
#[ignore = "spawns nested cargo builds; run by cargo xtask test-binding"]
fn twenty_real_nidl_mutations_close_the_binding_contract_end_to_end() {
    let root = artifact_root();
    clear_case_artifacts(&root);
    let shared_target = root.join("cargo-target");
    let base_idl = nexa_idl::parse(e2e_support::BASE_NIDL).expect("base NIDL parses");
    let base_contract_runtime_id = nexa_idl::contract_runtime_id(&base_idl);
    let base_generated = nexa_idl::generate_rust(&base_idl).expect("base bindings generate");
    let context = CaseContext {
        root: &root,
        shared_target: &shared_target,
        base_contract_runtime_id,
        base_generated: &base_generated,
    };
    let evidence = mutations()
        .iter()
        .map(|mutation| execute_mutation(&context, mutation))
        .collect::<Vec<_>>();
    assert_eq!(evidence.len(), 20);
    write_report(&root, &evidence);
}

struct CaseContext<'a> {
    root: &'a Path,
    shared_target: &'a Path,
    base_contract_runtime_id: StableId,
    base_generated: &'a str,
}

fn clear_case_artifacts(root: &Path) {
    fs::create_dir_all(root).expect("create IDL E2E artifact root");
    for entry in fs::read_dir(root).expect("read IDL E2E artifact root") {
        let entry = entry.expect("read IDL E2E artifact entry");
        if entry.file_name() == "cargo-target" {
            continue;
        }
        let file_type = entry.file_type().expect("read IDL E2E artifact type");
        if file_type.is_dir() {
            fs::remove_dir_all(entry.path()).expect("clear prior IDL E2E case directory");
        } else {
            fs::remove_file(entry.path()).expect("clear prior IDL E2E report");
        }
    }
}

fn execute_mutation(context: &CaseContext<'_>, mutation: &MutationCase) -> MutationEvidence {
    let changed_idl = nexa_idl::parse(&mutation.mutated_nidl)
        .unwrap_or_else(|error| panic!("{} must parse: {error}", mutation.name));
    let changed_contract_runtime_id = nexa_idl::contract_runtime_id(&changed_idl);
    if mutation.expected_changed_contract_runtime_id {
        assert_ne!(
            changed_contract_runtime_id, context.base_contract_runtime_id,
            "{}",
            mutation.name
        );
    }
    let first = nexa_idl::generate_rust(&changed_idl).expect("first binding generation");
    let second = nexa_idl::generate_rust(&changed_idl).expect("second binding generation");
    let third = nexa_idl::generate_rust(&changed_idl).expect("third binding generation");
    assert_eq!(first, second, "{} generation 1/2", mutation.name);
    assert_eq!(second, third, "{} generation 2/3", mutation.name);

    let case = prepare_case(
        context.root,
        mutation,
        &changed_idl,
        context.base_generated,
        &first,
    );
    let unchanged = check_business_host(&case, &first, BUSINESS_HOST_V1, context.shared_target);
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
    let patched_business_host_compiled = if mutation.unchanged_business_host_should_compile {
        assert_eq!(patched_host, BUSINESS_HOST_V1);
        unchanged.status.success()
    } else {
        assert_expected_business_diagnostic(mutation, &unchanged);
        assert_ne!(
            patched_host, BUSINESS_HOST_V1,
            "{} requires an explicit business patch",
            mutation.name
        );
        let patched = check_business_host(&case, &first, &patched_host, context.shared_target);
        assert!(
            patched.status.success(),
            "{} minimally patched BusinessHost must compile:\nstdout:\n{}\nstderr:\n{}",
            mutation.name,
            String::from_utf8_lossy(&patched.stdout),
            String::from_utf8_lossy(&patched.stderr)
        );
        patched.status.success()
    };

    let positive =
        run_generated_registry_positive(&case, &first, &patched_host, context.shared_target);
    assert!(
        positive.status.success(),
        "{} GeneratedHostRegistry positive runtime must pass:\nstdout:\n{}\nstderr:\n{}",
        mutation.name,
        String::from_utf8_lossy(&positive.stdout),
        String::from_utf8_lossy(&positive.stderr)
    );
    let runtime = read_runtime_evidence(&case);
    assert_runtime_evidence(&runtime, mutation);
    MutationEvidence {
        id: mutation.id,
        name: mutation.name,
        base_contract_runtime_id: context.base_contract_runtime_id,
        changed_contract_runtime_id,
        base_generated_hash: stable_bytes_hash(context.base_generated),
        changed_generated_hash: stable_bytes_hash(&first),
        unchanged_business_host_should_compile: mutation.unchanged_business_host_should_compile,
        patch_insertions,
        patch_deletions,
        old_bytecode_rejected: runtime.lifecycle.old_bytecode_rejected.is_observed(),
        positive_registry: "GeneratedHostRegistry",
        patched_business_host_compiled,
        changed_module_loaded: runtime.lifecycle.changed_module_loaded.is_observed(),
        heartbeat_result: runtime.heartbeat_result,
        runtime_terminal_record: runtime.lifecycle.runtime_terminal_record.is_observed(),
        runtime_ledger_balanced: runtime.lifecycle.runtime_ledger_balanced.is_observed(),
        affected_surface: runtime.affected.affected_surface,
        affected_surface_result: runtime.affected.affected_surface_result,
        affected_surface_pending: runtime.affected.affected_surface_pending,
        realm_release_records: runtime.affected.realm_release_records,
        affected_completion_records: runtime.affected.affected_completion_records,
        affected_release_records: runtime.affected.affected_release_records,
    }
}

fn assert_runtime_evidence(runtime: &RuntimeMutationEvidence, mutation: &MutationCase) {
    assert!(
        runtime.lifecycle.old_bytecode_rejected.is_observed(),
        "{}",
        mutation.name
    );
    assert!(
        runtime.lifecycle.changed_module_loaded.is_observed(),
        "{}",
        mutation.name
    );
    assert_eq!(runtime.heartbeat_result, 42, "{}", mutation.name);
    assert!(
        runtime.lifecycle.runtime_terminal_record.is_observed(),
        "{}",
        mutation.name
    );
    assert!(
        runtime.lifecycle.runtime_ledger_balanced.is_observed(),
        "{}",
        mutation.name
    );
    assert_eq!(
        runtime.affected.affected_surface,
        mutation.affected_surface(),
        "{}",
        mutation.name
    );
}
