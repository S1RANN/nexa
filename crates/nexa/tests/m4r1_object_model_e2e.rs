use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa::prelude::{
    Instruction, RealmConfig, RealmRuntime, RuntimeValue, ScopeHandle, StepConfig, TaskLimits,
    TaskPoll, YieldReason,
};
use nexa::{
    CandidateIdentity, HostContractInput, PackageBuildSession, SourceIdentity,
    canonical_host_contract_source_identity,
    canonical_package_build_fingerprint_input_with_contract,
};
use nexa_analysis::{
    CompilationLimits, NormalizedPackagePath, PackageManifest, ResolvedBuildInput,
    ResolvedDependencyGraph, ResolvedPackage, SourceId, SourceRole, SourceSetBuilder,
};

const PACKAGE_ID: &str = "test.object-model-e2e";
const MODULE: &str = "object.model.e2e";
const HOST_URI: &str = "nidl://tests/m4r1-object-model-e2e/empty.nidl";
const HOST_SOURCE: &str = r"
contract Empty {
    nexa {
        fn struct_value_copy() -> i32;
        fn enum_payload() -> i32;
        fn class_mutable_field() -> i32;
        fn class_identity() -> bool;
        fn option_class_some() -> i32;
        fn option_class_none() -> i32;
    }
}
";
const SOURCE: &str = r"
struct Point {
    x: i32,
    y: i32,
}

enum Measurement {
    Missing,
    Captured(Point),
}

class Counter {
    mut value: i32,
}

pub fn struct_value_copy() -> i32 {
    let original = Point { x: 2, y: 3 };
    let copied = original;
    let updated = Point { x: 10, ..copied };
    return original.x * 100 + updated.x * 10 + updated.y;
}

pub fn enum_payload() -> i32 {
    let measurement = Measurement::Captured(Point { x: 7, y: 5 });
    return match measurement {
        Measurement::Missing => 0,
        Measurement::Captured(point) => point.x * 10 + point.y,
    };
}

pub fn class_mutable_field() -> i32 {
    let original = new Counter { value: 1 };
    let alias = original;
    alias.value = 7;
    return original.value;
}

pub fn class_identity() -> bool {
    let original = new Counter { value: 11 };
    let alias = original;
    let distinct = new Counter { value: 11 };
    return original == alias && original != distinct;
}

pub fn option_class_some() -> i32 {
    let value: Option<Counter> = Option::Some(new Counter { value: 29 });
    return match value {
        Option::None => 0,
        Option::Some(counter) => counter.value,
    };
}

pub fn option_class_none() -> i32 {
    let value: Option<Counter> = Option::None;
    return match value {
        Option::None => 41,
        Option::Some(counter) => counter.value,
    };
}
";

fn resolved_input(contract: &HostContractInput<'_>) -> ResolvedBuildInput {
    let manifest = Arc::new(
        PackageManifest::parse(&format!(
            r#"
schema = 2
kind = "application"
id = "{PACKAGE_ID}"
name = "M4R1 object model end-to-end"
version = "1.0.0"
source_root = "src"
entry = "{MODULE}"
activation = "programmatic"
"#
        ))
        .expect("valid object-model fixture manifest"),
    );
    let mut source_builder =
        SourceSetBuilder::new(manifest.id.clone(), CompilationLimits::default());
    source_builder
        .add(
            NormalizedPackagePath::new("src/object/model/e2e.nexa")
                .expect("normalized fixture source path"),
            SOURCE,
            SourceRole::Production,
        )
        .expect("valid fixture source");
    let sources = Arc::new(source_builder.build().expect("valid fixture source set"));
    let graph = Arc::new(ResolvedDependencyGraph {
        root: manifest.id.clone(),
        packages: BTreeMap::from([(
            manifest.id.clone(),
            ResolvedPackage {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("m4r1-object-model-e2e").expect("valid source ID"),
                directory: NormalizedPackagePath::new("packages/object-model-e2e")
                    .expect("normalized package directory"),
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
    .expect("canonical resolved object-model input")
}

fn entrypoint_stable_id(contract: &nexa::ValidatedContract, name: &str) -> nexa::StableId {
    contract
        .nexa_functions
        .iter()
        .find(|entrypoint| entrypoint.name == name)
        .map_or_else(
            || panic!("missing NIDL Nexa entrypoint `{name}`"),
            nexa::entrypoint_stable_id,
        )
}

fn task_config(owner: ScopeHandle) -> StepConfig {
    StepConfig {
        owner,
        priority: 1,
        fuel_slice: 100_000,
        cumulative_budget: 500_000,
        limits: TaskLimits::default(),
    }
}

fn execute(
    realm: &mut RealmRuntime,
    module: nexa::prelude::ModuleHandle,
    contract: &nexa::ValidatedContract,
    owner: ScopeHandle,
    function: &str,
) -> RuntimeValue {
    let task = realm
        .spawn_task(
            module,
            entrypoint_stable_id(contract, function),
            &[],
            task_config(owner),
        )
        .unwrap_or_else(|error| panic!("spawn {function}: {error}"));
    for _ in 0..8 {
        match realm
            .poll_task(task, 100_000)
            .unwrap_or_else(|error| panic!("poll {function}: {error}"))
        {
            TaskPoll::Completed(value) => return value,
            TaskPoll::Yielded(YieldReason::Fuel) => {}
            other => panic!("{function} did not complete normally: {other:?}"),
        }
    }
    panic!("{function} exhausted the bounded test polling window");
}

#[test]
#[allow(clippy::too_many_lines)]
fn canonical_object_model_source_executes_through_verified_bytecode() {
    let parsed_contract = nexa::parse_contract(HOST_SOURCE).expect("valid empty NIDL contract");
    let contract = HostContractInput::with_source(
        &parsed_contract,
        SourceIdentity::standalone(HOST_URI),
        HOST_SOURCE,
    )
    .expect("exact fixture NIDL source");
    let input = resolved_input(&contract);
    let identity =
        CandidateIdentity::new(input.root_manifest.id.clone(), 1, input.build_fingerprint)
            .expect("valid candidate identity");
    let artifact = PackageBuildSession::new()
        .compile_package_with_contract(&input, &contract, identity)
        .expect("canonical source-to-typed-compiler build");
    artifact
        .verify_integrity()
        .expect("package artifact integrity");

    let instructions = artifact
        .module()
        .functions
        .iter()
        .flat_map(|function| function.code.iter())
        .collect::<Vec<_>>();
    for (name, present) in [
        (
            "StructNew",
            instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::StructNew { .. })),
        ),
        (
            "StructWith",
            instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::StructWith { .. })),
        ),
        (
            "EnumNew",
            instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::EnumNew { .. })),
        ),
        (
            "EnumPayload",
            instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::EnumPayload { .. })),
        ),
        (
            "ClassNew",
            instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::ClassNew { .. })),
        ),
        (
            "ClassGet",
            instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::ClassGet { .. })),
        ),
        (
            "ClassEqual",
            instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::ClassEqual { .. })),
        ),
    ] {
        assert!(
            present,
            "typed compiler omitted required {name} instruction"
        );
    }

    let contract_runtime_id = nexa::contract_runtime_id(&parsed_contract);
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm
        .load_module(
            artifact.verified.clone(),
            contract_runtime_id,
            artifact.state_schema_fingerprint,
        )
        .expect("verifier-approved artifact loads into Realm");
    let owner = realm.create_scope(None).expect("fixture task scope");
    nexa::profiler::disable();
    let _ = nexa::profiler::take_thread_report();
    nexa::profiler::enable();

    assert_eq!(
        execute(
            &mut realm,
            module,
            &parsed_contract,
            owner,
            "struct_value_copy",
        ),
        RuntimeValue::I32(303),
        "Struct update must copy the value and leave the original unchanged"
    );
    assert_eq!(
        execute(&mut realm, module, &parsed_contract, owner, "enum_payload"),
        RuntimeValue::I32(75)
    );
    assert_eq!(
        execute(
            &mut realm,
            module,
            &parsed_contract,
            owner,
            "class_mutable_field",
        ),
        RuntimeValue::I32(7),
        "Class copies must retain reference identity and expose mutable field writes"
    );
    assert_eq!(
        execute(
            &mut realm,
            module,
            &parsed_contract,
            owner,
            "class_identity"
        ),
        RuntimeValue::Bool(true),
        "Class equality must compare object identity"
    );
    assert_eq!(
        execute(
            &mut realm,
            module,
            &parsed_contract,
            owner,
            "option_class_some",
        ),
        RuntimeValue::I32(29),
        "Option<Class> Some payload must remain traceable and readable"
    );
    assert_eq!(
        execute(
            &mut realm,
            module,
            &parsed_contract,
            owner,
            "option_class_none",
        ),
        RuntimeValue::I32(41),
        "Option<Class> None must remain distinct from Some"
    );
    nexa::profiler::disable();
    let profile = nexa::profiler::take_thread_report().expect("profiled Package execution");
    assert_eq!(profile.dropped, nexa::DroppedProfile::default());
    assert!(!profile.allocations.is_empty());
    assert!(profile.allocations.iter().all(|allocation| {
        allocation.site.package_id == PACKAGE_ID
            && allocation.site.module == MODULE
            && allocation.site.function_stable_id.0 != 0
            && allocation.site.source_span.is_some()
    }));
    assert!(
        !profile.allocations.iter().any(|allocation| matches!(
            allocation.site.kind,
            nexa::AllocationKind::StructMaterialization
        )),
        "verified Struct values must remain in physical slots without heap materialization"
    );
    assert!(
        profile
            .allocations
            .iter()
            .any(|allocation| matches!(allocation.site.kind, nexa::AllocationKind::Class))
    );
    assert!(
        !profile.allocations.iter().any(|allocation| matches!(
            allocation.site.kind,
            nexa::AllocationKind::EnumMaterialization
        )),
        "verified Enum values must remain in physical slots without heap materialization"
    );
}
