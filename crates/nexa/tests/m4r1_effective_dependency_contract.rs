use std::collections::BTreeMap;
use std::sync::Arc;

use nexa::{
    HostContractInput, SourceIdentity, canonical_package_build_fingerprint_input_with_contract,
};
use nexa_analysis::{
    CompilationLimits, NormalizedPackagePath, PackageId, PackageManifest, PackageSourceSet,
    SourceRole, SourceSetBuilder,
};

const CONTRACT_SOURCE: &str = r"
contract DependencyContract;
struct Payload {
    value: i32,
}
host {
    fn amber() -> Payload;
    fn cobalt() -> Payload;
    fn violet() -> Payload;
}
";

fn root_manifest() -> PackageManifest {
    PackageManifest::parse(
        r#"
schema = 2
kind = "application"
id = "effective.root"
name = "Effective Contract Root"
version = "1.0.0"
source_root = "src"
entry = "app"
activation = "programmatic"

[dependencies]
support = { path = "../support" }
"#,
    )
    .expect("root manifest")
}

fn dependency_manifest() -> PackageManifest {
    PackageManifest::parse(
        r#"
schema = 2
kind = "library"
id = "effective.support"
name = "Effective Contract Support"
version = "1.0.0"
source_root = "src"
"#,
    )
    .expect("dependency manifest")
}

fn source_set(
    package: &PackageId,
    path: &str,
    source: impl Into<Arc<str>>,
) -> Arc<PackageSourceSet> {
    let mut builder = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
    builder
        .add(
            NormalizedPackagePath::new(path).expect("normalized fixture source path"),
            source,
            SourceRole::Production,
        )
        .expect("fixture source");
    Arc::new(builder.build().expect("fixture source set"))
}

#[test]
#[allow(clippy::too_many_lines)]
fn static_library_host_reference_defines_the_effective_contract() {
    let parsed_contract = nexa::parse_contract(CONTRACT_SOURCE).expect("fixture NIDL");
    let contract = HostContractInput::with_source(
        &parsed_contract,
        SourceIdentity::standalone(
            "nidl://tests/m4r1-effective-dependency/dependency-contract.nidl",
        ),
        CONTRACT_SOURCE,
    )
    .expect("exact fixture NIDL source");

    let mut stable_sorted_functions = parsed_contract.host_functions.iter().collect::<Vec<_>>();
    stable_sorted_functions.sort_by(|left, right| {
        left.stable_id
            .cmp(&right.stable_id)
            .then_with(|| left.name.cmp(&right.name))
    });
    let selected_function = stable_sorted_functions
        .last()
        .expect("fixture declares Host functions");
    assert_ne!(
        selected_function.stable_id, stable_sorted_functions[0].stable_id,
        "the dependency must reference a Host function which is not first in stable-ID order"
    );

    let root_manifest = root_manifest();
    assert_eq!(
        root_manifest.dependencies.len(),
        1,
        "the library is the root's unique and therefore first static dependency"
    );
    let root_sources = source_set(
        &root_manifest.id,
        "src/app.nexa",
        "pub fn root_value() -> i32 { return 7; }\n",
    );
    let dependency_manifest = Arc::new(dependency_manifest());
    let dependency_source = format!(
        "pub fn load() -> host::Payload {{ return host::{}(); }}\n",
        selected_function.name
    );
    let dependency_sources = source_set(
        &dependency_manifest.id,
        "src/support.nexa",
        dependency_source,
    );
    let dependency_manifests = BTreeMap::from([(
        dependency_manifest.id.clone(),
        Arc::clone(&dependency_manifest),
    )]);
    let dependency_source_sets = BTreeMap::from([(
        dependency_manifest.id.clone(),
        Arc::clone(&dependency_sources),
    )]);

    let effective = contract
        .selecting_effective_package_contract(
            &root_manifest,
            &root_sources,
            &dependency_source_sets,
        )
        .expect("dependency Host reference must define the effective Contract");
    assert_eq!(
        effective
            .effective_descriptor()
            .host_functions
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect::<Vec<_>>(),
        vec![selected_function.name.as_str()]
    );
    assert_eq!(
        effective
            .effective_descriptor()
            .shared_types
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Payload"],
        "the selected Host function's nominal result type must be effective"
    );

    let fingerprint_input = canonical_package_build_fingerprint_input_with_contract(
        &root_manifest,
        &root_sources,
        &dependency_manifests,
        &dependency_source_sets,
        &contract,
        None,
    );
    assert_eq!(
        fingerprint_input.host_contract,
        effective.effective_descriptor().bytes,
        "canonical build identity must bind the same dependency-selected descriptor"
    );

    let dependency_without_host = source_set(
        &dependency_manifest.id,
        "src/support.nexa",
        "pub fn local_value() -> i32 { return 11; }\n",
    );
    let dependency_source_sets_without_host =
        BTreeMap::from([(dependency_manifest.id.clone(), dependency_without_host)]);
    let effective_without_host = contract
        .selecting_effective_package_contract(
            &root_manifest,
            &root_sources,
            &dependency_source_sets_without_host,
        )
        .expect("Host-free dependency has an empty effective Host surface");
    let fingerprint_without_host = canonical_package_build_fingerprint_input_with_contract(
        &root_manifest,
        &root_sources,
        &dependency_manifests,
        &dependency_source_sets_without_host,
        &contract,
        None,
    );

    assert!(
        effective_without_host
            .effective_descriptor()
            .host_functions
            .is_empty()
    );
    assert!(
        effective_without_host
            .effective_descriptor()
            .shared_types
            .is_empty()
    );
    assert_ne!(
        fingerprint_input.host_contract,
        fingerprint_without_host.host_contract
    );
    assert_ne!(
        effective.effective_contract_fingerprint(),
        effective_without_host.effective_contract_fingerprint()
    );
}
