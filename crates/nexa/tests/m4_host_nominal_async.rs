use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa::{
    CandidateIdentity, HostContractInput, PackageBuildSession, SourceIdentity,
    canonical_host_contract_source_identity,
    canonical_package_build_fingerprint_input_with_contract,
};
use nexa_analysis::{
    CompilationLimits, NormalizedPackagePath, PackageManifest, ResolvedBuildInput,
    ResolvedDependencyGraph, ResolvedPackage, SourceId, SourceRole, SourceSetBuilder,
};
use nexa_bytecode::{HostCallMode, ValueType};

const PACKAGE_ID: &str = "host.nominal.fixture";
const MODULE: &str = "host.nominal.fixture";
const HOST_URI: &str = "nidl://tests/m4-host-nominal-async/nominal-host.nidl";
const HOST: &str = r"
contract NominalHost {
    handle Ticket;
    struct Payload { ticket: Ticket, label: string, }
    enum Failure { Cancelled, Abandoned, Missing(Ticket), }
    host {
        fn issue() -> Ticket;
        fn inspect(ticket: Ticket) -> Payload;
        @cancel(return_error)
        @abandon(trap)
        async fn fetch(ticket: Ticket) -> Result<Payload, Failure>;
    }
}
";
const SOURCE: &str = r"
use host::nominal_host as api;

fn echo_ticket(value: api::Ticket) -> api::Ticket {
    return value;
}

fn inspect_payload(value: api::Ticket) -> api::Payload {
    return api::inspect(value);
}

async fn fetch_payload(value: api::Ticket) -> Result<api::Payload, api::Failure> {
    return api::fetch(value).await;
}
";

fn resolved_input(contract: &HostContractInput<'_>) -> ResolvedBuildInput {
    let manifest = Arc::new(
        PackageManifest::parse(&format!(
            r#"
schema = 2
kind = "application"
id = "{PACKAGE_ID}"
name = "Host nominal fixture"
version = "1.0.0"
source_root = "src"
entry = "{MODULE}"
activation = "programmatic"
"#
        ))
        .expect("fixture manifest"),
    );
    let mut source_builder =
        SourceSetBuilder::new(manifest.id.clone(), CompilationLimits::default());
    source_builder
        .add(
            NormalizedPackagePath::new("src/host/nominal/fixture.nexa").expect("source path"),
            SOURCE,
            SourceRole::Production,
        )
        .expect("fixture source");
    let sources = Arc::new(source_builder.build().expect("fixture source set"));
    let graph = Arc::new(ResolvedDependencyGraph {
        root: manifest.id.clone(),
        packages: BTreeMap::from([(
            manifest.id.clone(),
            ResolvedPackage {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("host-nominal-canonical").expect("source ID"),
                directory: NormalizedPackagePath::new("packages/host-nominal")
                    .expect("package directory"),
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let fingerprint_input = canonical_package_build_fingerprint_input_with_contract(
        &manifest,
        &sources,
        &BTreeMap::new(),
        &BTreeMap::new(),
        contract,
        None,
    );
    let canonical_host_contract = fingerprint_input.host_contract.clone();
    let host_contract_source = canonical_host_contract_source_identity(contract);
    let host_required_entrypoints = fingerprint_input.host_required_entrypoints.clone();
    ResolvedBuildInput::new(
        manifest,
        sources,
        BTreeMap::new(),
        BTreeMap::new(),
        graph,
        None,
        Arc::<[u8]>::from(canonical_host_contract),
        Arc::<[u8]>::from(host_contract_source),
        Arc::<[u8]>::from(host_required_entrypoints),
        nexa_analysis::CompilationOptions::default(),
        fingerprint_input,
    )
    .expect("resolved canonical fixture")
}

fn function_signature(
    artifact: &nexa::CompiledPackageArtifact,
    name: &str,
) -> nexa_bytecode::Signature {
    artifact
        .debug_inspection()
        .functions
        .iter()
        .find(|function| function.module_path == MODULE && function.name == name)
        .and_then(|function| function.signature.clone())
        .unwrap_or_else(|| panic!("missing function signature for {name}"))
}

#[test]
#[allow(clippy::too_many_lines)]
fn canonical_build_preserves_host_nominals_and_async_result_arms() {
    let parsed_contract = nexa::parse_nidl(HOST).expect("fixture NIDL");
    let contract = HostContractInput::with_source(
        &parsed_contract,
        SourceIdentity::standalone(HOST_URI),
        HOST,
    )
    .expect("exact fixture NIDL source");
    let input = resolved_input(&contract);
    let identity =
        CandidateIdentity::new(input.root_manifest.id.clone(), 1, input.build_fingerprint)
            .expect("candidate identity");
    let artifact = PackageBuildSession::new()
        .compile_package_with_contract(&input, &contract, identity)
        .expect("canonical typed build");
    artifact.verify_integrity().expect("artifact integrity");

    let ticket = parsed_contract
        .handles
        .iter()
        .find(|declaration| declaration.name == "Ticket")
        .expect("Ticket declaration")
        .stable_id;
    let payload = parsed_contract
        .structs
        .iter()
        .find(|declaration| declaration.name == "Payload")
        .expect("Payload declaration")
        .stable_id;
    let failure = parsed_contract
        .enums
        .iter()
        .find(|declaration| declaration.name == "Failure")
        .expect("Failure declaration")
        .stable_id;
    let issue = parsed_contract
        .host_functions
        .iter()
        .find(|function| function.name == "issue")
        .expect("issue declaration")
        .stable_id;
    let inspect = parsed_contract
        .host_functions
        .iter()
        .find(|function| function.name == "inspect")
        .expect("inspect declaration")
        .stable_id;
    let fetch = parsed_contract
        .host_functions
        .iter()
        .find(|function| function.name == "fetch")
        .expect("fetch declaration")
        .stable_id;
    let payload_type = artifact
        .module()
        .struct_types
        .iter()
        .find(|ty| ty.type_id == payload)
        .expect("Host struct bytecode layout");
    assert_eq!(
        payload_type
            .fields
            .iter()
            .map(|field| field.ty)
            .collect::<Vec<_>>(),
        [ValueType::Named(ticket), ValueType::String]
    );
    let failure_type = artifact
        .module()
        .enum_types
        .iter()
        .find(|ty| ty.type_id == failure)
        .expect("Host enum bytecode layout");
    assert_eq!(failure_type.variants.len(), 3);
    assert_eq!(failure_type.variants[2].tag, 2);
    assert_eq!(
        failure_type.variants[2].payload_type,
        Some(ValueType::Named(ticket))
    );

    let imports = &artifact.module().host_imports;
    assert_eq!(
        imports.len(),
        2,
        "the effective Contract must emit only referenced Host functions"
    );
    assert!(
        imports.iter().all(|import| import.stable_id != issue),
        "the unreferenced issue function must not widen the bytecode import set"
    );
    let inspect_import = imports
        .iter()
        .find(|import| import.stable_id == inspect)
        .expect("referenced inspect Host import");
    assert_eq!(
        (inspect_import.parameters.as_slice(), inspect_import.result),
        (
            [ValueType::Named(ticket)].as_slice(),
            Some(ValueType::Named(payload))
        )
    );
    let fetch_import = imports
        .iter()
        .find(|import| import.stable_id == fetch)
        .expect("referenced fetch Host import");
    assert_eq!(fetch_import.mode, HostCallMode::Async);
    let async_result = fetch_import
        .async_result
        .expect("typed async Result metadata");
    assert_eq!(async_result.success, ValueType::Named(payload));
    assert_eq!(async_result.error, ValueType::Named(failure));
    let expected_result =
        nexa_bytecode::result_type(ValueType::Named(payload), ValueType::Named(failure));
    assert_eq!(async_result.result_type, expected_result.type_id);
    assert_eq!(
        fetch_import.result,
        Some(ValueType::Named(expected_result.type_id))
    );
    assert!(
        artifact
            .module()
            .enum_types
            .iter()
            .any(|ty| ty == &expected_result),
        "verifier-visible Result<S, E> metadata must be emitted"
    );

    assert_eq!(
        function_signature(&artifact, "echo_ticket"),
        nexa_bytecode::Signature {
            parameters: vec![ValueType::Named(ticket)],
            result: Some(ValueType::Named(ticket)),
        }
    );
    assert_eq!(
        function_signature(&artifact, "inspect_payload"),
        nexa_bytecode::Signature {
            parameters: vec![ValueType::Named(ticket)],
            result: Some(ValueType::Named(payload)),
        }
    );
    assert_eq!(
        function_signature(&artifact, "fetch_payload"),
        nexa_bytecode::Signature {
            parameters: vec![ValueType::Named(ticket)],
            result: Some(ValueType::Named(expected_result.type_id)),
        },
        "await Host request must produce Result<Payload, Failure>, never Unit"
    );
}
