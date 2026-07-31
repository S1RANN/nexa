use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa::{CandidateIdentity, PackageBuildSession, canonical_package_build_fingerprint_input};
use nexa_analysis::{
    CompilationLimits, NormalizedPackagePath, PackageManifest, ResolvedBuildInput,
    ResolvedDependencyGraph, ResolvedPackage, SourceId, SourceRole, SourceSetBuilder,
};
use nexa_bytecode::{HostCallMode, ValueType};
use nexa_core::StableId;

const PACKAGE_ID: &str = "host.nominal.fixture";
const MODULE: &str = "host.nominal.fixture";
const HOST: &str = r"
interface NominalHost {
    opaque Ticket;
    struct Payload { ticket: Ticket; label: string; }
    enum Failure { Cancelled, Abandoned, Missing(Ticket) }
    sync fn issue() -> Ticket;
    sync fn inspect(ticket: Ticket) -> Payload;
    request(cancel_task, trap) fn fetch(ticket: Ticket)
        -> request<Result<Payload, Failure>>;
}
";
const SOURCE: &str = r"
module host.nominal.fixture;
import host as api;

fn echo_ticket(value: api.Ticket) -> api.Ticket {
    return value;
}

fn inspect_payload(value: api.Ticket) -> api.Payload {
    return api.inspect(value);
}

task fn fetch_payload(value: api.Ticket) -> Result<api.Payload, api.Failure> {
    return await api.fetch(value);
}
";

fn resolved_input(contract: &nexa::Idl) -> ResolvedBuildInput {
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
    let fingerprint_input = canonical_package_build_fingerprint_input(
        &manifest,
        &sources,
        &BTreeMap::new(),
        &BTreeMap::new(),
        contract,
        None,
    );
    let host_contract_source = fingerprint_input.host_contract_source.clone();
    let host_required_exports = fingerprint_input.host_required_exports.clone();
    ResolvedBuildInput::new(
        manifest,
        sources,
        BTreeMap::new(),
        BTreeMap::new(),
        graph,
        None,
        Arc::<[u8]>::from(nexa::canonical_idl(contract).into_bytes()),
        Arc::<[u8]>::from(host_contract_source),
        Arc::<[u8]>::from(host_required_exports),
        nexa_analysis::CompilationOptions::default(),
        fingerprint_input,
    )
    .expect("resolved canonical fixture")
}

fn function_signature<'artifact>(
    artifact: &'artifact nexa::CompiledPackageArtifact,
    name: &str,
) -> &'artifact nexa_bytecode::Signature {
    let function = artifact
        .debug_info
        .functions
        .iter()
        .find(|function| function.module_path == MODULE && function.name == name)
        .unwrap_or_else(|| panic!("missing function {name}"));
    &artifact.module().functions
        [usize::try_from(function.function_index).expect("function index fits usize")]
    .signature
}

#[test]
fn canonical_build_preserves_host_nominals_and_async_result_arms() {
    let contract = nexa::parse_idl(HOST).expect("fixture NIDL");
    let input = resolved_input(&contract);
    let identity =
        CandidateIdentity::new(input.root_manifest.id.clone(), 1, input.build_fingerprint)
            .expect("candidate identity");
    let artifact = PackageBuildSession::new()
        .compile_package(&input, &contract, identity)
        .expect("canonical typed build");
    artifact.verify_integrity().expect("artifact integrity");

    let ticket = StableId::from_name("Ticket");
    let payload = StableId::from_name("Payload");
    let failure = StableId::from_name("Failure");
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
    assert_eq!(imports.len(), 3);
    assert_eq!(imports[0].result, Some(ValueType::Named(ticket)));
    assert_eq!(
        (imports[1].parameters.as_slice(), imports[1].result),
        (
            [ValueType::Named(ticket)].as_slice(),
            Some(ValueType::Named(payload))
        )
    );
    assert_eq!(imports[2].mode, HostCallMode::Async);
    let async_result = imports[2]
        .async_result
        .expect("typed async Result metadata");
    assert_eq!(async_result.success, ValueType::Named(payload));
    assert_eq!(async_result.error, ValueType::Named(failure));
    let expected_result =
        nexa_bytecode::result_type(ValueType::Named(payload), ValueType::Named(failure));
    assert_eq!(async_result.result_type, expected_result.type_id);
    assert_eq!(
        imports[2].result,
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
        &nexa_bytecode::Signature {
            parameters: vec![ValueType::Named(ticket)],
            result: Some(ValueType::Named(ticket)),
        }
    );
    assert_eq!(
        function_signature(&artifact, "inspect_payload"),
        &nexa_bytecode::Signature {
            parameters: vec![ValueType::Named(ticket)],
            result: Some(ValueType::Named(payload)),
        }
    );
    assert_eq!(
        function_signature(&artifact, "fetch_payload"),
        &nexa_bytecode::Signature {
            parameters: vec![ValueType::Named(ticket)],
            result: Some(ValueType::Named(expected_result.type_id)),
        },
        "await Host request must produce Result<Payload, Failure>, never Unit"
    );
}
