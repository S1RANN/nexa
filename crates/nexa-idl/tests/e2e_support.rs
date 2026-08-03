use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nexa_core::StableId;
use serde_json::Value;

pub const BASE_NIDL: &str = include_str!("fixtures/business_host/contract.nidl");
pub const BUSINESS_HOST_V1: &str = include_str!("fixtures/business_host/business_host.rs");
const BASE_NEXA_MODULE: &str = "use host::game as engine;\n\
pub fn update(entity: i32) -> i32 { return engine::heartbeat(entity); }\n\
fn reset() -> i32 { return 0; }\n";
const GENERATED_REGISTRY_RUNTIME_TEST: &str = r#"
use super::*;
use nexa_runtime::HostRegistry;
use std::fmt::Write as _;

#[test]
fn changed_binding_executes_through_generated_registry() {
    let base_idl = nexa_idl::parse(include_str!("base_contract.nidl")).expect("base NIDL");
    let changed_idl = nexa_idl::parse(include_str!("contract.nidl")).expect("changed NIDL");
    assert_eq!(
        CONTRACT_RUNTIME_ID,
        nexa_idl::contract_runtime_id(&changed_idl)
    );
    let base_source = include_str!("base_module.nexa");
    let changed_source = include_str!("module.nexa");
    let old_module =
        nexa_compiler::compile_with_contract(base_source, &base_idl).expect("old module");
    let old_state_schema_fingerprint = old_module.module().state_schema_fingerprint;
    let changed_module =
        nexa_compiler::compile_with_contract(changed_source, &changed_idl)
            .expect("changed module");
    let changed_state_schema_fingerprint = changed_module.module().state_schema_fingerprint;
    let affected_stable_id = {
        let binding_model =
            nexa_idl::BindingModel::from_contract(&changed_idl).expect("changed binding model");
        let changed_bytecode = changed_module.module();
        assert_eq!(
            changed_bytecode.host_contract_id,
            Some(CONTRACT_RUNTIME_ID)
        );
        __AFFECTED_IMPORT_ASSERTIONS__
    };

    let runtime_host = nexa_runtime::RuntimeHost::new(8);
    let registry = GeneratedHostRegistry::new(BusinessHostV1);
    let mut realm = nexa_runtime::RealmRuntime::hosted(
        nexa_runtime::RealmConfig::default(),
        runtime_host.clone(),
        Box::new(registry),
    )
    .expect("hosted changed Registry");

    let before = realm.inspection_snapshot();
    let old_load = realm.load_module(
        old_module,
        CONTRACT_RUNTIME_ID,
        old_state_schema_fingerprint,
    );
    let old_bytecode_rejected = matches!(
        old_load,
        Err(nexa_runtime::RealmError::HostContractIdMismatch)
    );
    assert!(old_bytecode_rejected);
    let rejected = realm.inspection_snapshot();
    assert_eq!(rejected.active_root, before.active_root);
    assert_eq!(rejected.modules.len(), before.modules.len());
    assert!(rejected.tasks.is_empty());
    assert!(rejected.terminal_tasks.is_empty());

    let module = realm
        .load_module(
            changed_module,
            CONTRACT_RUNTIME_ID,
            changed_state_schema_fingerprint,
        )
        .expect("changed module loads");
    let loaded = realm.inspection_snapshot();
    let changed_module_loaded = loaded
        .modules
        .iter()
        .any(|loaded_module| loaded_module.handle == module);
    assert!(changed_module_loaded);

    let scope = realm.create_scope(None).expect("runtime evidence scope");
    let task = realm
        .spawn_export::<Update>(
            module,
            &UpdateArgs { entity: 41 },
            nexa_runtime::StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 64,
                cumulative_budget: 1_024,
                limits: nexa_runtime::TaskLimits::default(),
            },
        )
        .expect("spawn generated Update entrypoint");
    let heartbeat_result = match realm
        .poll_task(task, 64)
        .expect("poll generated Update entrypoint")
    {
        nexa_runtime::TaskPoll::Completed(nexa_runtime::RuntimeValue::I32(value)) => value,
        other => panic!("generated Update entrypoint did not complete: {other:?}"),
    };
    assert_eq!(heartbeat_result, 42);
    let runtime_terminal_record = realm.terminal_record(task).is_some();
    assert!(runtime_terminal_record);

    let affected_surface_result;
    let affected_surface_pending;
    __AFFECTED_REALM_PROBE__

    realm.cancel_scope(scope).expect("close runtime evidence scope");
    realm
        .destroy_empty_scope(scope)
        .expect("destroy runtime evidence scope");
    let realm_ledger_balanced = realm.resource_ledger().is_zero();
    assert!(realm_ledger_balanced, "{:?}", realm.resource_ledger());
    drop(realm);
    let realm_release_records = runtime_host.drain_releases().len();
    let _ = runtime_host.begin_close();
    runtime_host.try_finish_close().expect("close changed Host");
    let host_ledger_balanced = runtime_host.resource_ledger().is_zero();
    assert!(host_ledger_balanced);

    let mut probe_tasks =
        nexa_runtime::TaskRuntime::new(2, nexa_runtime::RuntimeLimits::default());
    let probe_scope = probe_tasks.create_scope(None).expect("probe scope");
    let probe_task = probe_tasks
        .admit_task(probe_scope, 1, true)
        .expect("probe task");
    let mut probe_resources = nexa_runtime::RuntimeResources::new(1, 8, 16);
    let mut probe_heap = nexa_runtime::Heap::new(16);
    let mut probe_registry = GeneratedHostRegistry::new(BusinessHostV1);
    __AFFECTED_REGISTRY_PROBE__
    probe_resources
        .cleanup_task(probe_task, false)
        .expect("cleanup affected Registry probe");
    let affected_completion_records = probe_resources.drain_completions().len();
    let affected_release_records = probe_resources.drain_releases().len();
    let probe_ledger_balanced = probe_resources.resource_ledger().is_zero();
    assert!(
        probe_ledger_balanced,
        "{:?}",
        probe_resources.resource_ledger()
    );
    let runtime_ledger_balanced =
        realm_ledger_balanced && host_ledger_balanced && probe_ledger_balanced;

    let mut evidence = String::new();
    write!(
        evidence,
        "{{\"old_bytecode_rejected\":{old_bytecode_rejected},\
         \"changed_module_loaded\":{changed_module_loaded},\
         \"heartbeat_result\":{heartbeat_result},\
         \"runtime_terminal_record\":{runtime_terminal_record},\
         \"runtime_ledger_balanced\":{runtime_ledger_balanced},\
         \"affected_surface\":\"__AFFECTED_SURFACE__\",\
         \"affected_surface_result\":{affected_surface_result},\
         \"affected_surface_pending\":{affected_surface_pending},\
         \"realm_release_records\":{realm_release_records},\
         \"affected_completion_records\":{affected_completion_records},\
         \"affected_release_records\":{affected_release_records}}}"
    )
    .expect("write runtime evidence");
    std::fs::write(
        std::env::var_os("NEXA_IDL_E2E_EVIDENCE").expect("runtime evidence path"),
        evidence,
    )
    .expect("persist runtime evidence");
}
"#;

#[derive(Clone, Copy, Debug)]
enum MutationProbe {
    FunctionRename,
    ParameterTypeChange,
    ReturnTypeChange,
    SyncToAsync,
    FuelCostChange,
    CancelPolicyChange,
    AbandonPolicyChange,
    EnumVariantRename,
    StructFieldRename,
    SnapshotContentTypeChange,
    ResourceTokenDomainChange,
    ContractRename,
    HandleRename,
    StructRename,
    EnumRename,
    EnumPayloadTypeChange,
    ParameterRename,
    ParameterAddition,
    StructFieldAddition,
    EntrypointAddition,
}

#[derive(Clone, Copy)]
enum ProbeOutput {
    I32(i32),
    I64(i64),
    PendingRequest,
    ResourceToken,
    Snapshot {
        handle_type: &'static str,
        encoder_type: &'static str,
    },
}

impl MutationProbe {
    fn for_id(id: &str) -> Self {
        match id {
            "01" => Self::FunctionRename,
            "02" => Self::ParameterTypeChange,
            "03" => Self::ReturnTypeChange,
            "04" => Self::SyncToAsync,
            "05" => Self::FuelCostChange,
            "06" => Self::CancelPolicyChange,
            "07" => Self::AbandonPolicyChange,
            "08" => Self::EnumVariantRename,
            "09" => Self::StructFieldRename,
            "10" => Self::SnapshotContentTypeChange,
            "11" => Self::ResourceTokenDomainChange,
            "12" => Self::ContractRename,
            "13" => Self::HandleRename,
            "14" => Self::StructRename,
            "15" => Self::EnumRename,
            "16" => Self::EnumPayloadTypeChange,
            "17" => Self::ParameterRename,
            "18" => Self::ParameterAddition,
            "19" => Self::StructFieldAddition,
            "20" => Self::EntrypointAddition,
            _ => panic!("mutation must have a concrete generated-surface probe: {id}"),
        }
    }

    const fn affected_surface(self) -> &'static str {
        match self {
            Self::FunctionRename => "registry:GameHost.advance",
            Self::ParameterTypeChange
            | Self::ReturnTypeChange
            | Self::FuelCostChange
            | Self::ParameterRename
            | Self::ParameterAddition => "registry:GameHost.update",
            Self::SyncToAsync => "registry:GameHost.update(async)",
            Self::CancelPolicyChange => "registry:GameHost.animation(cancel_task)",
            Self::AbandonPolicyChange => "registry:GameHost.animation(return_error)",
            Self::EnumVariantRename => "registry:AnimationError.MissingAsset",
            Self::StructFieldRename => "registry:EnemyView.hit_points",
            Self::SnapshotContentTypeChange => "registry:EnemyStateSnapshot",
            Self::ResourceTokenDomainChange => "registry:MotionLockToken",
            Self::ContractRename => "registry:CombatHost.update",
            Self::HandleRename => "registry:Actor",
            Self::StructRename => "registry:EnemyStats",
            Self::EnumRename => "registry:PlaybackError",
            Self::EnumPayloadTypeChange => "registry:AnimationError.Code(i64)",
            Self::StructFieldAddition => "registry:EnemyView.armor",
            Self::EntrypointAddition => "entrypoint:reset",
        }
    }

    fn import_assertions(self) -> String {
        if matches!(self, Self::EntrypointAddition) {
            return r#"
        let binding_entrypoint = binding_model
            .nexa_functions
            .iter()
            .find(|candidate| candidate.identity.source_name == "reset")
            .expect("changed binding model contains reset");
        let validated_entrypoint = changed_idl
            .nexa_functions
            .iter()
            .find(|candidate| candidate.name == "reset")
            .expect("changed contract contains reset");
        assert_eq!(
            binding_entrypoint.identity.stable_id,
            validated_entrypoint.stable_id
        );
        assert_eq!(Reset::STABLE_ID, validated_entrypoint.stable_id);
        assert!(
            changed_bytecode
                .exports
                .iter()
                .any(|entrypoint| entrypoint.stable_id == Reset::STABLE_ID),
            "changed bytecode must publish the generated Reset entrypoint marker"
        );
        assert_eq!(Reset::NAME, "reset");
        assert_eq!(
            <Reset as nexa_runtime::ScriptExport>::effect(),
            nexa_runtime::FunctionEffect::Ordinary
        );
        binding_entrypoint.identity.stable_id
"#
            .to_owned();
        }
        let function = self.host_function_name();
        let is_async = matches!(
            self,
            Self::SyncToAsync | Self::CancelPolicyChange | Self::AbandonPolicyChange
        );
        let mut assertions = format!(
            r#"
        let affected_function = binding_model
            .host_functions
            .iter()
            .find(|candidate| candidate.identity.source_name == {function:?})
            .expect("changed binding model contains the affected Host function");
        let validated_function = changed_idl
            .host_functions
            .iter()
            .find(|candidate| candidate.name == {function:?})
            .expect("changed contract contains the affected Host function");
        assert_eq!(affected_function.identity.source_name, validated_function.name);
        assert_eq!(affected_function.identity.stable_id, validated_function.stable_id);
        assert_eq!(affected_function.is_async, {is_async});
"#
        );
        match self {
            Self::SyncToAsync => assertions.push_str(
                r"
        assert!(affected_function.is_async);
",
            ),
            Self::FuelCostChange => assertions.push_str(
                r"
        assert_eq!(affected_function.fuel_cost, 7);
",
            ),
            Self::CancelPolicyChange => assertions.push_str(
                r"
        assert_eq!(
            affected_function.cancel_policy,
            nexa_idl::CancelPolicy::CancelTask
        );
",
            ),
            Self::AbandonPolicyChange => assertions.push_str(
                r"
        assert_eq!(
            affected_function.abandon_policy,
            nexa_idl::AbandonPolicy::ReturnError
        );
",
            ),
            _ => {}
        }
        assertions.push_str(
            r"
        affected_function.identity.stable_id
",
        );
        assertions
    }

    fn realm_probe(self) -> &'static str {
        if matches!(self, Self::EntrypointAddition) {
            r#"
    assert_eq!(affected_stable_id, Reset::STABLE_ID);
    let reset_task = realm
        .spawn_export::<Reset>(
            module,
            &ResetArgs,
            nexa_runtime::StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 64,
                cumulative_budget: 1_024,
                limits: nexa_runtime::TaskLimits::default(),
            },
        )
        .expect("spawn generated Reset entrypoint");
    affected_surface_result = match realm
        .poll_task(reset_task, 64)
        .expect("poll generated Reset entrypoint")
    {
        nexa_runtime::TaskPoll::Completed(nexa_runtime::RuntimeValue::I32(value)) => {
            i64::from(value)
        }
        other => panic!("generated Reset entrypoint did not complete: {other:?}"),
    };
    affected_surface_pending = false;
    assert!(realm.terminal_record(reset_task).is_some());
"#
        } else {
            ""
        }
    }

    fn registry_probe(self, contract: &nexa_idl::ValidatedContract) -> String {
        if matches!(self, Self::EntrypointAddition) {
            return String::new();
        }
        let arguments = self.registry_arguments(contract);
        let outcome = self.probe_output();
        let outcome_check = match outcome {
            ProbeOutput::I32(expected) => format!(
                r"
        nexa_runtime::HostCallOutcome::RuntimeImmediate(
            nexa_runtime::RuntimeValue::I32(value),
        ) => {{
            assert_eq!(value, {expected});
            affected_surface_result = i64::from(value);
            affected_surface_pending = false;
        }}
"
            ),
            ProbeOutput::I64(expected) => format!(
                r"
        nexa_runtime::HostCallOutcome::RuntimeImmediate(
            nexa_runtime::RuntimeValue::I64(value),
        ) => {{
            assert_eq!(value, {expected});
            affected_surface_result = value;
            affected_surface_pending = false;
        }}
"
            ),
            ProbeOutput::PendingRequest => r#"
        nexa_runtime::HostCallOutcome::Pending(request) => {
            assert!(probe_resources.owns_request(probe_task, request));
            let live_requests = probe_resources.resource_ledger().requests;
            assert_eq!(live_requests, 1);
            affected_surface_result =
                i64::try_from(live_requests).expect("request count fits i64");
            affected_surface_pending = true;
        }
"#
            .to_owned(),
            ProbeOutput::ResourceToken => r#"
        nexa_runtime::HostCallOutcome::RuntimeImmediate(
            nexa_runtime::RuntimeValue::ResourceToken(_token),
        ) => {
            let live_tokens = probe_resources.resource_ledger().tokens;
            assert_eq!(live_tokens, 1);
            affected_surface_result =
                i64::try_from(live_tokens).expect("token count fits i64");
            affected_surface_pending = false;
        }
"#
            .to_owned(),
            ProbeOutput::Snapshot {
                handle_type,
                encoder_type,
            } => format!(
                r#"
        nexa_runtime::HostCallOutcome::RuntimeImmediate(
            nexa_runtime::RuntimeValue::Snapshot(snapshot),
        ) => {{
            assert_eq!(snapshot.type_id(), {handle_type}::TYPE_ID);
            assert_eq!(
                probe_resources
                    .snapshot_content_type(snapshot)
                    .expect("snapshot content type"),
                {encoder_type}::CONTENT_TYPE
            );
            let live_snapshots = probe_resources.resource_ledger().snapshots;
            assert_eq!(live_snapshots, 1);
            affected_surface_result =
                i64::try_from(live_snapshots).expect("snapshot count fits i64");
            affected_surface_pending = false;
        }}
"#
            ),
        };
        format!(
            r#"
    let probe_values = {arguments};
    let affected_slot = probe_registry.resolve_function(affected_stable_id).expect("affected Registry binding").slot();
    let probe_outcome = {{
        let mut probe_context = probe_resources.context(probe_task, 0, 1);
        probe_registry
            .call_runtime(
                affected_slot,
                &mut probe_context,
                nexa_runtime::RuntimeHostArgs::new(
                    &probe_values,
                    Some(&mut probe_heap),
                )
                .expect("affected Registry arguments"),
            )
            .expect("affected generated Registry call")
    }};
    match probe_outcome {{
{outcome_check}
        other => panic!("unexpected affected Registry outcome: {{other:?}}"),
    }}
"#
        )
    }

    const fn host_function_name(self) -> &'static str {
        match self {
            Self::FunctionRename => "advance",
            Self::ParameterTypeChange
            | Self::ReturnTypeChange
            | Self::SyncToAsync
            | Self::FuelCostChange
            | Self::ContractRename
            | Self::ParameterRename
            | Self::ParameterAddition => "update",
            Self::CancelPolicyChange | Self::AbandonPolicyChange => "animation",
            Self::EnumVariantRename | Self::EnumRename | Self::EnumPayloadTypeChange => "classify",
            Self::StructFieldRename | Self::StructRename | Self::StructFieldAddition => "score",
            Self::SnapshotContentTypeChange => "view",
            Self::ResourceTokenDomainChange => "lock",
            Self::HandleRename => "inspect",
            Self::EntrypointAddition => panic!("Reset is an entrypoint, not a Host function"),
        }
    }

    fn registry_arguments(self, contract: &nexa_idl::ValidatedContract) -> String {
        match self {
            Self::FunctionRename
            | Self::ReturnTypeChange
            | Self::SyncToAsync
            | Self::FuelCostChange
            | Self::ContractRename
            | Self::ParameterRename => "vec![nexa_runtime::RuntimeValue::I32(40), \
                 nexa_runtime::RuntimeValue::I32(2)]"
                .to_owned(),
            Self::ParameterTypeChange => "vec![nexa_runtime::RuntimeValue::I64(40), \
                 nexa_runtime::RuntimeValue::I32(2)]"
                .to_owned(),
            Self::CancelPolicyChange
            | Self::AbandonPolicyChange
            | Self::ResourceTokenDomainChange => {
                "vec![nexa_runtime::RuntimeValue::I32(41)]".to_owned()
            }
            Self::EnumVariantRename | Self::EnumRename | Self::EnumPayloadTypeChange => {
                self.enum_registry_arguments(contract)
            }
            Self::StructFieldRename | Self::StructRename | Self::StructFieldAddition => {
                self.struct_registry_arguments(contract)
            }
            Self::SnapshotContentTypeChange => "Vec::new()".to_owned(),
            Self::HandleRename => {
                let type_id = contract
                    .handles
                    .iter()
                    .find(|handle| handle.name == "Actor")
                    .expect("handle-rename contract contains Actor")
                    .stable_id
                    .0;
                format!(
                    r"vec![nexa_runtime::RuntimeValue::Opaque {{
        value: 41,
        type_id: nexa_runtime::StableId({type_id}),
    }}]"
                )
            }
            Self::ParameterAddition => "vec![nexa_runtime::RuntimeValue::I32(39), \
                 nexa_runtime::RuntimeValue::I32(1), \
                 nexa_runtime::RuntimeValue::I32(2)]"
                .to_owned(),
            Self::EntrypointAddition => {
                panic!("Reset uses its generated entrypoint marker")
            }
        }
    }

    fn enum_registry_arguments(self, contract: &nexa_idl::ValidatedContract) -> String {
        let (enum_name, variant_name, payload, expectation) = match self {
            Self::EnumVariantRename => (
                "AnimationError",
                "MissingAsset",
                "None",
                "MissingAsset enum value",
            ),
            Self::EnumRename => (
                "PlaybackError",
                "MissingClip",
                "None",
                "PlaybackError enum value",
            ),
            Self::EnumPayloadTypeChange => (
                "AnimationError",
                "Code",
                "Some(nexa_runtime::RuntimeValue::I64(41))",
                "AnimationError Code(i64) value",
            ),
            _ => panic!("mutation does not probe a generated enum conversion"),
        };
        let enumeration = contract
            .enums
            .iter()
            .find(|enumeration| enumeration.name == enum_name)
            .unwrap_or_else(|| panic!("mutated contract contains enum {enum_name}"));
        let (tag, variant) = enumeration
            .variants
            .iter()
            .enumerate()
            .find(|(_, variant)| variant.name == variant_name)
            .unwrap_or_else(|| panic!("{enum_name} contains variant {variant_name}"));
        let type_id = enumeration.stable_id.0;
        let variant_id = variant.stable_id.0;
        format!(
            r"{{
        let value = probe_heap
            .allocate_enum(
                nexa_runtime::StableId({type_id}),
                nexa_runtime::StableId({variant_id}),
                {tag},
                {payload},
            )
            .expect({expectation:?});
        vec![value]
    }}"
        )
    }

    fn struct_registry_arguments(self, contract: &nexa_idl::ValidatedContract) -> String {
        let (struct_name, values, expectation) = match self {
            Self::StructFieldRename => (
                "EnemyView",
                "&[nexa_runtime::RuntimeValue::I32(40)]",
                "EnemyView hit_points value",
            ),
            Self::StructRename => (
                "EnemyStats",
                "&[nexa_runtime::RuntimeValue::I32(40)]",
                "EnemyStats value",
            ),
            Self::StructFieldAddition => (
                "EnemyView",
                "&[\n                    nexa_runtime::RuntimeValue::I32(40),\n                    \
                 nexa_runtime::RuntimeValue::I32(2),\n                ]",
                "EnemyView health and armor value",
            ),
            _ => panic!("mutation does not probe a generated struct conversion"),
        };
        let type_id = contract
            .structs
            .iter()
            .find(|structure| structure.name == struct_name)
            .unwrap_or_else(|| panic!("mutated contract contains struct {struct_name}"))
            .stable_id
            .0;
        format!(
            r"{{
        let value = probe_heap
            .allocate_struct(
                nexa_runtime::StableId({type_id}),
                {values},
            )
            .expect({expectation:?});
        vec![value]
    }}"
        )
    }

    const fn probe_output(self) -> ProbeOutput {
        match self {
            Self::FunctionRename
            | Self::ParameterTypeChange
            | Self::FuelCostChange
            | Self::ContractRename
            | Self::ParameterRename
            | Self::ParameterAddition => ProbeOutput::I32(42),
            Self::ReturnTypeChange => ProbeOutput::I64(42),
            Self::SyncToAsync | Self::CancelPolicyChange | Self::AbandonPolicyChange => {
                ProbeOutput::PendingRequest
            }
            Self::EnumVariantRename | Self::EnumRename => ProbeOutput::I32(1),
            Self::StructFieldRename | Self::StructRename | Self::StructFieldAddition => {
                ProbeOutput::I32(40)
            }
            Self::SnapshotContentTypeChange => ProbeOutput::Snapshot {
                handle_type: "EnemyStateSnapshot",
                encoder_type: "EnemyStateSnapshotEncoder",
            },
            Self::ResourceTokenDomainChange => ProbeOutput::ResourceToken,
            Self::HandleRename | Self::EnumPayloadTypeChange => ProbeOutput::I32(41),
            Self::EntrypointAddition => {
                panic!("Reset result is observed through RealmRuntime")
            }
        }
    }
}

pub struct MutationCase {
    pub id: &'static str,
    pub name: &'static str,
    pub mutated_nidl: String,
    pub unchanged_business_host_should_compile: bool,
    pub expected_diagnostic_symbols: &'static [&'static str],
    pub patch_business_host: fn(&str) -> String,
    pub expected_changed_contract_runtime_id: bool,
    probe: MutationProbe,
}

#[allow(clippy::struct_excessive_bools)]
pub struct MutationEvidence {
    pub id: &'static str,
    pub name: &'static str,
    pub base_contract_runtime_id: StableId,
    pub changed_contract_runtime_id: StableId,
    pub base_generated_hash: u64,
    pub changed_generated_hash: u64,
    pub unchanged_business_host_should_compile: bool,
    pub patch_insertions: usize,
    pub patch_deletions: usize,
    pub old_bytecode_rejected: bool,
    pub positive_registry: &'static str,
    pub patched_business_host_compiled: bool,
    pub changed_module_loaded: bool,
    pub heartbeat_result: i32,
    pub runtime_terminal_record: bool,
    pub runtime_ledger_balanced: bool,
    pub affected_surface: String,
    pub affected_surface_result: i64,
    pub affected_surface_pending: bool,
    pub realm_release_records: usize,
    pub affected_completion_records: usize,
    pub affected_release_records: usize,
}

pub struct RuntimeMutationEvidence {
    pub lifecycle: RuntimeLifecycleEvidence,
    pub heartbeat_result: i32,
    pub affected: AffectedSurfaceEvidence,
}

pub struct RuntimeLifecycleEvidence {
    pub old_bytecode_rejected: RuntimeObservation,
    pub changed_module_loaded: RuntimeObservation,
    pub runtime_terminal_record: RuntimeObservation,
    pub runtime_ledger_balanced: RuntimeObservation,
}

pub struct AffectedSurfaceEvidence {
    pub affected_surface: String,
    pub affected_surface_result: i64,
    pub affected_surface_pending: bool,
    pub realm_release_records: usize,
    pub affected_completion_records: usize,
    pub affected_release_records: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeObservation {
    Observed,
    Missing,
}

impl RuntimeObservation {
    const fn from_bool(observed: bool) -> Self {
        if observed {
            Self::Observed
        } else {
            Self::Missing
        }
    }

    #[must_use]
    pub const fn is_observed(self) -> bool {
        matches!(self, Self::Observed)
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn mutations() -> Vec<MutationCase> {
    vec![
        changed(
            "01",
            "function-rename",
            "fn update(",
            "fn advance(",
            false,
            &["update", "advance"],
            patch_function_rename,
        ),
        changed(
            "02",
            "parameter-type-change",
            "fn update(entity: i32, delta: i32) -> i32;",
            "fn update(entity: i64, delta: i32) -> i32;",
            false,
            &["update", "i64"],
            patch_parameter_type,
        ),
        changed(
            "03",
            "return-type-change",
            "update(entity: i32, delta: i32) -> i32",
            "update(entity: i32, delta: i32) -> i64",
            false,
            &["update", "i64"],
            patch_return_type,
        ),
        changed(
            "04",
            "sync-to-async",
            "        @fuel(5)\n        fn update(entity: i32, delta: i32) -> i32;",
            "        @fuel(5)\n        @cancel(return_error)\n        @abandon(trap)\n        async fn update(entity: i32, delta: i32) -> Result<i32, AnimationError>;",
            false,
            &["update", "HostRequestHandle"],
            patch_sync_to_async,
        ),
        changed(
            "05",
            "fuel-cost-change",
            "@fuel(5)",
            "@fuel(7)",
            true,
            &[],
            identity_patch,
        ),
        changed(
            "06",
            "cancel-policy-change",
            "@cancel(return_error)",
            "@cancel(cancel_task)",
            true,
            &[],
            identity_patch,
        ),
        changed(
            "07",
            "abandon-policy-change",
            "@abandon(trap)",
            "@abandon(return_error)",
            true,
            &[],
            identity_patch,
        ),
        changed(
            "08",
            "enum-variant-rename",
            "MissingClip",
            "MissingAsset",
            false,
            &["MissingClip", "AnimationError"],
            patch_variant_rename,
        ),
        changed(
            "09",
            "struct-field-rename",
            "    struct EnemyView {\n        health: i32,\n    }",
            "    struct EnemyView {\n        hit_points: i32,\n    }",
            false,
            &["health", "EnemyView"],
            patch_field_rename,
        ),
        changed(
            "10",
            "snapshot-content-type-change",
            "Snapshot<EnemyView>",
            "Snapshot<EnemyState>",
            false,
            &["EnemyViewSnapshot", "EnemyStateSnapshot"],
            patch_snapshot_content,
        ),
        changed(
            "11",
            "resource-token-domain-change",
            "Token<ActionLock>",
            "Token<MotionLock>",
            false,
            &["ActionLockToken", "MotionLockToken"],
            patch_token_domain,
        ),
        changed(
            "12",
            "contract-rename",
            "contract Game {",
            "contract Combat {",
            false,
            &["GameHost", "CombatHost"],
            patch_contract_rename,
        ),
        changed(
            "13",
            "handle-rename",
            "handle Entity;",
            "handle Actor;",
            false,
            &["Entity", "Actor"],
            patch_handle_rename,
        )
        .with_replacement("inspect(entity: Entity)", "inspect(entity: Actor)"),
        changed(
            "14",
            "struct-rename",
            "struct EnemyView",
            "struct EnemyStats",
            false,
            &["EnemyView", "EnemyStats"],
            patch_struct_rename,
        )
        .with_replacement("Snapshot<EnemyView>", "Snapshot<EnemyStats>")
        .with_replacement("score(view: EnemyView)", "score(view: EnemyStats)"),
        changed(
            "15",
            "enum-rename",
            "enum AnimationError",
            "enum PlaybackError",
            false,
            &["AnimationError", "PlaybackError"],
            patch_enum_rename,
        )
        .with_replacement("Result<i32, AnimationError>", "Result<i32, PlaybackError>")
        .with_replacement(
            "classify(error: AnimationError)",
            "classify(error: PlaybackError)",
        ),
        changed(
            "16",
            "enum-payload-type-change",
            "Code(i32)",
            "Code(i64)",
            false,
            &["Code", "i64"],
            patch_payload_type,
        ),
        changed(
            "17",
            "parameter-rename",
            "fn update(entity: i32, delta: i32) -> i32;",
            "fn update(entity: i32, step: i32) -> i32;",
            true,
            &[],
            identity_patch,
        ),
        changed(
            "18",
            "parameter-addition",
            "update(entity: i32, delta: i32)",
            "update(entity: i32, delta: i32, flags: i32)",
            false,
            &["update", "flags"],
            patch_parameter_addition,
        ),
        changed(
            "19",
            "struct-field-addition",
            "    struct EnemyView {\n        health: i32,\n    }",
            "    struct EnemyView {\n        health: i32,\n        armor: i32,\n    }",
            false,
            &["EnemyView", "armor"],
            patch_struct_field_addition,
        ),
        changed(
            "20",
            "entrypoint-addition",
            "    nexa {\n        fn update(entity: i32) -> i32;\n    }",
            "    nexa {\n        fn update(entity: i32) -> i32;\n        fn reset() -> i32;\n    }",
            true,
            &[],
            identity_patch,
        ),
    ]
}

#[test]
fn nidl_v2_mutation_needles_match_the_multiline_contract_fixture() {
    let mutations = mutations();
    assert_eq!(mutations.len(), 20);
    for mutation in mutations {
        assert_ne!(mutation.mutated_nidl, BASE_NIDL, "{}", mutation.name);
        nexa_idl::parse(&mutation.mutated_nidl)
            .unwrap_or_else(|error| panic!("{} must remain valid NIDL v2: {error}", mutation.name));
    }
}

impl MutationCase {
    #[must_use]
    pub const fn affected_surface(&self) -> &'static str {
        self.probe.affected_surface()
    }

    fn with_replacement(mut self, from: &str, to: &str) -> Self {
        assert!(
            self.mutated_nidl.contains(from),
            "missing mutation token {from}"
        );
        self.mutated_nidl = self.mutated_nidl.replacen(from, to, 1);
        self
    }
}

fn changed(
    id: &'static str,
    name: &'static str,
    from: &str,
    to: &str,
    unchanged_business_host_should_compile: bool,
    expected_diagnostic_symbols: &'static [&'static str],
    patch_business_host: fn(&str) -> String,
) -> MutationCase {
    assert!(BASE_NIDL.contains(from), "missing mutation token {from}");
    MutationCase {
        id,
        name,
        mutated_nidl: BASE_NIDL.replacen(from, to, 1),
        unchanged_business_host_should_compile,
        expected_diagnostic_symbols,
        patch_business_host,
        expected_changed_contract_runtime_id: true,
        probe: MutationProbe::for_id(id),
    }
}

fn identity_patch(source: &str) -> String {
    source.to_owned()
}

fn patch_function_rename(source: &str) -> String {
    source.replacen("    fn update(", "    fn advance(", 1)
}

fn patch_parameter_type(source: &str) -> String {
    source
        .replacen(
            "        entity: i32,\n        delta: i32,",
            "        entity: i64,\n        delta: i32,",
            1,
        )
        .replacen(
            "Ok(entity + delta)",
            "Ok((entity + i64::from(delta)) as i32)",
            1,
        )
}

fn patch_return_type(source: &str) -> String {
    source.replacen(
        "    ) -> Result<i32, HostError> {\n        Ok(entity + delta)",
        "    ) -> Result<i64, HostError> {\n        Ok(i64::from(entity + delta))",
        1,
    )
}

fn patch_sync_to_async(source: &str) -> String {
    replace_block(
        source,
        "    fn update(",
        "    fn animation(",
        r"    fn update(
        &mut self,
        context: &mut nexa_runtime::ResourceContext<'_>,
        _entity: i32,
        _delta: i32,
    ) -> Result<nexa_runtime::HostRequestHandle, HostError> {
        context
            .create_request()
            .map(|pending| pending.request)
            .map_err(|error| HostError(error.to_string()))
    }

",
    )
}

fn patch_variant_rename(source: &str) -> String {
    source.replacen(
        "AnimationErrorRef::MissingClip",
        "AnimationErrorRef::MissingAsset",
        1,
    )
}

fn patch_field_rename(source: &str) -> String {
    source
        .replace("EnemyView { health: 40 }", "EnemyView { hit_points: 40 }")
        .replace("view.health()", "view.hit_points()")
}

fn patch_snapshot_content(source: &str) -> String {
    replace_block(
        source,
        "    fn view(",
        "    fn inspect(",
        r#"    fn view(
        &mut self,
        context: &mut nexa_runtime::ResourceContext<'_>,
    ) -> Result<EnemyStateSnapshot, HostError> {
        let encoded = EnemyStateSnapshotEncoder::encode(&EnemyState { health: 40 })?;
        let handle = context
            .create_typed_snapshot(encoded)
            .map_err(|error| HostError(error.to_string()))?;
        EnemyStateSnapshot::try_from_raw(handle)
            .map_err(|error| HostError(format!("{error:?}")))
    }

"#,
    )
}

fn patch_token_domain(source: &str) -> String {
    source
        .replacen(
            "Result<ActionLockToken, HostError>",
            "Result<MotionLockToken, HostError>",
            1,
        )
        .replacen(
            "ActionLockToken::CONTENT_TYPE_ID",
            "MotionLockToken::CONTENT_TYPE_ID",
            1,
        )
        .replacen(
            "ActionLockToken::try_from_raw",
            "MotionLockToken::try_from_raw",
            1,
        )
}

fn patch_contract_rename(source: &str) -> String {
    source.replacen(
        "impl GameHost for BusinessHostV1",
        "impl CombatHost for BusinessHostV1",
        1,
    )
}

fn patch_handle_rename(source: &str) -> String {
    source.replacen("        entity: Entity,", "        entity: Actor,", 1)
}

fn patch_struct_rename(source: &str) -> String {
    source.replace("EnemyView", "EnemyStats")
}

fn patch_enum_rename(source: &str) -> String {
    source.replace("AnimationErrorRef", "PlaybackErrorRef")
}

fn patch_payload_type(source: &str) -> String {
    source.replacen(
        "AnimationErrorRef::Code(code) => code,",
        "AnimationErrorRef::Code(code) => i32::try_from(code).unwrap_or_default(),",
        1,
    )
}

fn patch_parameter_addition(source: &str) -> String {
    source
        .replacen(
            "        delta: i32,\n    )",
            "        delta: i32,\n        flags: i32,\n    )",
            1,
        )
        .replacen("Ok(entity + delta)", "Ok(entity + delta + flags)", 1)
}

fn patch_struct_field_addition(source: &str) -> String {
    source.replacen(
        "EnemyView { health: 40 }",
        "EnemyView { health: 40, armor: 2 }",
        1,
    )
}

fn replace_block(source: &str, start: &str, end: &str, replacement: &str) -> String {
    let start_index = source.find(start).expect("business Host block start");
    let end_index = source[start_index..]
        .find(end)
        .map(|offset| start_index + offset)
        .expect("business Host block end");
    format!(
        "{}{}{}",
        &source[..start_index],
        replacement,
        &source[end_index..]
    )
}

#[must_use]
pub fn artifact_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nexa-artifacts/idl-e2e")
}

#[must_use]
pub fn prepare_case(
    root: &Path,
    mutation: &MutationCase,
    changed_contract: &nexa_idl::ValidatedContract,
    base_generated: &str,
    changed_generated: &str,
) -> PathBuf {
    let case = root.join(format!("{}-{}", mutation.id, mutation.name));
    for directory in ["base", "mutated", "host/src", "script"] {
        fs::create_dir_all(case.join(directory)).expect("create E2E artifact directory");
    }
    fs::write(case.join("base/contract.nidl"), BASE_NIDL).expect("write base NIDL");
    fs::write(case.join("base/bindings.rs"), base_generated).expect("write base binding");
    fs::write(case.join("mutated/contract.nidl"), &mutation.mutated_nidl)
        .expect("write changed NIDL");
    for (index, generated) in [changed_generated, changed_generated, changed_generated]
        .iter()
        .enumerate()
    {
        fs::write(
            case.join(format!("mutated/binding-{}.rs", index + 1)),
            generated,
        )
        .expect("write deterministic binding");
    }
    let changed_module = positive_module_source(mutation);
    fs::write(case.join("script/module.nexa"), &changed_module).expect("write positive script");
    fs::write(case.join("host/src/base_contract.nidl"), BASE_NIDL)
        .expect("write base contract fixture");
    fs::write(case.join("host/src/contract.nidl"), &mutation.mutated_nidl)
        .expect("write changed contract fixture");
    fs::write(case.join("host/src/base_module.nexa"), BASE_NEXA_MODULE)
        .expect("write base script fixture");
    fs::write(case.join("host/src/module.nexa"), changed_module)
        .expect("write changed script fixture");
    let crate_path = |name: &str| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../{name}"))
            .canonicalize()
            .unwrap_or_else(|error| panic!("{name} path: {error}"))
    };
    let runtime = crate_path("nexa-runtime");
    let compiler = crate_path("nexa-compiler");
    let bytecode = crate_path("nexa-bytecode");
    let idl = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("IDL path");
    let verifier = crate_path("nexa-verifier");
    fs::write(
        case.join("host/Cargo.toml"),
        format!(
            "[package]\nname=\"idl-e2e-{}\"\nversion=\"0.0.0\"\nedition=\"2024\"\n\
             [workspace]\n[dependencies]\n\
             nexa-runtime={{path=\"{}\",features=[\"model-adapter\"]}}\n\
             nexa-compiler={{path=\"{}\"}}\n\
             nexa-bytecode={{path=\"{}\"}}\n\
             nexa-idl={{path=\"{}\"}}\n\
             nexa-verifier={{path=\"{}\"}}\n",
            mutation.id,
            runtime.display(),
            compiler.display(),
            bytecode.display(),
            idl.display(),
            verifier.display(),
        ),
    )
    .expect("write host Cargo manifest");
    fs::write(
        case.join("host/src/lib.rs"),
        "include!(\"bindings.rs\");\ninclude!(\"business_host.rs\");\n\
         #[cfg(test)] mod runtime_test;\n",
    )
    .expect("write host crate root");
    let runtime_test = GENERATED_REGISTRY_RUNTIME_TEST
        .replace(
            "__AFFECTED_IMPORT_ASSERTIONS__",
            &mutation.probe.import_assertions(),
        )
        .replace("__AFFECTED_REALM_PROBE__", mutation.probe.realm_probe())
        .replace(
            "__AFFECTED_REGISTRY_PROBE__",
            &mutation.probe.registry_probe(changed_contract),
        )
        .replace("__AFFECTED_SURFACE__", mutation.probe.affected_surface());
    assert!(
        !runtime_test.contains("__AFFECTED_"),
        "{} runtime test has an unresolved probe placeholder",
        mutation.name
    );
    fs::write(case.join("host/src/runtime_test.rs"), runtime_test)
        .expect("write generated Registry runtime test");
    case
}

fn positive_module_source(mutation: &MutationCase) -> String {
    match mutation.id {
        "12" => BASE_NEXA_MODULE.replacen("use host::game", "use host::combat", 1),
        "20" => BASE_NEXA_MODULE.replacen(
            "fn reset() -> i32 { return 0; }",
            "pub fn reset() -> i32 { return 0; }",
            1,
        ),
        _ => BASE_NEXA_MODULE.to_owned(),
    }
}

#[must_use]
pub fn check_business_host(
    case: &Path,
    changed_generated: &str,
    business_host: &str,
    shared_target: &Path,
) -> Output {
    fs::write(case.join("host/src/bindings.rs"), changed_generated).expect("write bindings");
    fs::write(case.join("host/src/business_host.rs"), business_host).expect("write business Host");
    Command::new("cargo")
        .args(["+1.97.1", "check", "--offline", "--message-format=json"])
        .env("CARGO_TARGET_DIR", shared_target)
        .current_dir(case.join("host"))
        .output()
        .expect("run business Host cargo check")
}

#[must_use]
pub fn run_generated_registry_positive(
    case: &Path,
    changed_generated: &str,
    patched_business_host: &str,
    shared_target: &Path,
) -> Output {
    fs::write(case.join("host/src/bindings.rs"), changed_generated).expect("write bindings");
    fs::write(
        case.join("host/src/business_host.rs"),
        patched_business_host,
    )
    .expect("write patched business Host");
    Command::new("cargo")
        .args(["+1.97.1", "test", "--offline", "--message-format=json"])
        .env("CARGO_TARGET_DIR", shared_target)
        .env("NEXA_IDL_E2E_EVIDENCE", case.join("runtime-evidence.json"))
        .current_dir(case.join("host"))
        .output()
        .expect("run generated Registry positive test")
}

#[must_use]
pub fn read_runtime_evidence(case: &Path) -> RuntimeMutationEvidence {
    let path = case.join("runtime-evidence.json");
    let value: Value = serde_json::from_slice(
        &fs::read(&path)
            .unwrap_or_else(|error| panic!("read runtime evidence {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("parse runtime evidence {}: {error}", path.display()));
    let boolean = |field: &str| {
        value[field]
            .as_bool()
            .unwrap_or_else(|| panic!("{field} must be a boolean in {}", path.display()))
    };
    let signed = |field: &str| {
        value[field]
            .as_i64()
            .unwrap_or_else(|| panic!("{field} must be an integer in {}", path.display()))
    };
    let unsigned = |field: &str| {
        usize::try_from(
            value[field]
                .as_u64()
                .unwrap_or_else(|| panic!("{field} must be unsigned in {}", path.display())),
        )
        .unwrap_or_else(|_| panic!("{field} must fit usize in {}", path.display()))
    };
    RuntimeMutationEvidence {
        lifecycle: RuntimeLifecycleEvidence {
            old_bytecode_rejected: RuntimeObservation::from_bool(boolean("old_bytecode_rejected")),
            changed_module_loaded: RuntimeObservation::from_bool(boolean("changed_module_loaded")),
            runtime_terminal_record: RuntimeObservation::from_bool(boolean(
                "runtime_terminal_record",
            )),
            runtime_ledger_balanced: RuntimeObservation::from_bool(boolean(
                "runtime_ledger_balanced",
            )),
        },
        heartbeat_result: i32::try_from(signed("heartbeat_result"))
            .expect("heartbeat result fits i32"),
        affected: AffectedSurfaceEvidence {
            affected_surface: value["affected_surface"]
                .as_str()
                .unwrap_or_else(|| panic!("affected_surface must be text in {}", path.display()))
                .to_owned(),
            affected_surface_result: signed("affected_surface_result"),
            affected_surface_pending: boolean("affected_surface_pending"),
            realm_release_records: unsigned("realm_release_records"),
            affected_completion_records: unsigned("affected_completion_records"),
            affected_release_records: unsigned("affected_release_records"),
        },
    }
}

pub fn assert_expected_business_diagnostic(mutation: &MutationCase, output: &Output) {
    let mut business_diagnostic = false;
    let mut symbol_diagnostic = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["reason"] != "compiler-message" {
            continue;
        }
        let message = &value["message"];
        let in_business_host = message["spans"].as_array().is_some_and(|spans| {
            spans.iter().any(|span| {
                span["file_name"]
                    .as_str()
                    .is_some_and(|file| file.ends_with("business_host.rs"))
            })
        });
        if !in_business_host {
            continue;
        }
        business_diagnostic = true;
        let diagnostic = format!(
            "{}\n{}",
            message["message"].as_str().unwrap_or_default(),
            message["rendered"].as_str().unwrap_or_default()
        );
        symbol_diagnostic |= mutation
            .expected_diagnostic_symbols
            .iter()
            .any(|symbol| diagnostic.contains(symbol));
    }
    assert!(
        business_diagnostic,
        "{} must fail in business_host.rs, stdout:\n{}",
        mutation.name,
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        symbol_diagnostic,
        "{} diagnostic must name one of {:?}",
        mutation.name, mutation.expected_diagnostic_symbols
    );
}

#[must_use]
pub fn patch_delta(before: &str, after: &str) -> (usize, usize) {
    let before = before.lines().collect::<Vec<_>>();
    let after = after.lines().collect::<Vec<_>>();
    let mut previous = vec![0usize; after.len() + 1];
    let mut current = vec![0usize; after.len() + 1];
    for left in &before {
        current[0] = 0;
        for (right_index, right) in after.iter().enumerate() {
            current[right_index + 1] = if left == right {
                previous[right_index] + 1
            } else {
                previous[right_index + 1].max(current[right_index])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let unchanged = previous[after.len()];
    (after.len() - unchanged, before.len() - unchanged)
}

#[test]
fn patch_delta_tracks_insertions_without_shifting_the_remaining_file() {
    assert_eq!(patch_delta("a\nb\nc\n", "a\ninserted\nb\nc\n"), (1, 0));
    assert_eq!(patch_delta("a\nold\nc\n", "a\nnew\nc\n"), (1, 1));
    assert_eq!(patch_delta("a\nremoved\nb\nc\n", "a\nb\nc\n"), (0, 1));
}

#[must_use]
pub fn stable_bytes_hash(bytes: &str) -> u64 {
    bytes.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

pub fn write_report(root: &Path, evidence: &[MutationEvidence]) {
    let mut contract_runtime_ids = BTreeSet::new();
    let mut rows = String::new();
    for (index, item) in evidence.iter().enumerate() {
        assert!(
            contract_runtime_ids.insert(item.changed_contract_runtime_id),
            "mutations must have distinct contract runtime IDs"
        );
        if index != 0 {
            rows.push_str(",\n");
        }
        write!(
            rows,
            "    {{\"id\":\"{}\",\"name\":\"{}\",\"base_contract_runtime_id\":\"{:016x}\",\
             \"changed_contract_runtime_id\":\"{:016x}\",\"base_generated_hash\":\"{:016x}\",\
             \"changed_generated_hash\":\"{:016x}\",\"unchanged_business_host_should_compile\":{},\
             \"patch_insertions\":{},\"patch_deletions\":{},\"old_bytecode_rejected\":{},\
             \"positive_registry\":\"{}\",\"patched_business_host_compiled\":{},\
             \"changed_module_loaded\":{},\"heartbeat_result\":{},\
             \"runtime_terminal_record\":{},\"runtime_ledger_balanced\":{},\
             \"affected_surface\":\"{}\",\"affected_surface_result\":{},\
             \"affected_surface_pending\":{},\"realm_release_records\":{},\
             \"affected_completion_records\":{},\"affected_release_records\":{}}}",
            item.id,
            item.name,
            item.base_contract_runtime_id.0,
            item.changed_contract_runtime_id.0,
            item.base_generated_hash,
            item.changed_generated_hash,
            item.unchanged_business_host_should_compile,
            item.patch_insertions,
            item.patch_deletions,
            item.old_bytecode_rejected,
            item.positive_registry,
            item.patched_business_host_compiled,
            item.changed_module_loaded,
            item.heartbeat_result,
            item.runtime_terminal_record,
            item.runtime_ledger_balanced,
            item.affected_surface,
            item.affected_surface_result,
            item.affected_surface_pending,
            item.realm_release_records,
            item.affected_completion_records,
            item.affected_release_records,
        )
        .expect("write JSON evidence row");
    }
    fs::write(
        root.join("mutation-report.json"),
        format!(
            "{{\n  \"schema_version\":3,\n  \"business_host\":\"BusinessHostV1\",\
             \n  \"mutation_count\":{},\
             \n  \"generated_registry_positive_runs\":{},\
             \n  \"manual_registry_positive_runs\":0,\n  \"status\":\"PASS\",\
             \n  \"mutations\":[\n{rows}\n  ]\n}}\n",
            evidence.len(),
            evidence.len()
        ),
    )
    .expect("write mutation report");
}
