use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use nexa_core::{CanonicalSymbolIdentity, StableId, SymbolKind};
use nexa_embed::{
    ActivationPolicy, ActivationSet, CandidateBuildContext, CapabilitySet, MemoryPackage,
    MemorySource, PackagePolicy, PackageRuntimeLimits, PackageSource, SourceId, TrustLevel,
};
use nexa_runtime::{
    HostPayload, ModuleLifecycle, RealmConfig, RealmRuntime, ReleaseKind, ResourceContext,
    RestartReloadOutcome, RestartReloadPolicy, RuntimeFailurePoint, RuntimeHost, RuntimeHostDomain,
    RuntimeValue, StepConfig, TaskLimits, TaskPoll, TickBudget,
};

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/combat_api.rs"));
}

struct EngineHost {
    last_request: Arc<Mutex<Option<nexa_runtime::PendingHostRequest>>>,
    last_token: Arc<Mutex<Option<nexa_runtime::ResourceTokenHandle>>>,
    last_snapshot: Arc<Mutex<Option<nexa_runtime::SnapshotHandle>>>,
}

const COMBAT_PACKAGE_ID: &str = "example.combat";
const COMBAT_HOST_SOURCE: &str = "examples/combat-runtime/combat_api.contract.nexa";

fn combat_symbol(kind: SymbolKind, name: &str) -> StableId {
    CanonicalSymbolIdentity::automatic(COMBAT_PACKAGE_ID, "game.combat", kind, name)
        .runtime_id()
        .0
}

fn compile_combat_candidate(
    session: &mut nexa::PackageBuildSession,
    contract: &nexa::HostContractInput<'_>,
    generation: u64,
    source_text: &str,
) -> Result<nexa::CompiledPackageArtifact, Box<dyn std::error::Error>> {
    let package_id = nexa::PackageId::new(COMBAT_PACKAGE_ID)?;
    let policy = PackagePolicy {
        trust: TrustLevel::FirstParty,
        capability_ceiling: CapabilitySet::default(),
        allowed_activation: ActivationSet::new([ActivationPolicy::Programmatic]),
        max_packages: 1,
        runtime_limits: PackageRuntimeLimits::default(),
        allow_entitlement: false,
    };
    let source = MemorySource::new(SourceId::new("combat-runtime")?, policy).package(
        MemoryPackage::new(
            "combat",
            format!(
                "schema = 2\n\
                 kind = \"application\"\n\
                 id = \"{COMBAT_PACKAGE_ID}\"\n\
                 name = \"Combat Runtime\"\n\
                 version = \"1.0.0\"\n\
                 source_root = \"src\"\n\
                 entry = \"game.combat\"\n\
                 activation = \"programmatic\"\n\
                 capabilities = []\n\
                 handler_fuel = 20000\n"
            ),
        )
        .source("src/game/combat.nexa", source_text),
    );
    let context = CandidateBuildContext::with_source(
        contract.source().identity().clone(),
        contract.source().text().as_bytes().to_vec(),
    );
    let discovered = source
        .discover(&context)?
        .into_iter()
        .next()
        .ok_or(HostFailure(
            "combat Package discovery returned no application",
        ))?;
    let identity = discovered.candidate.identity(generation)?;
    if identity.package_id != package_id {
        return Err(Box::new(HostFailure(
            "combat Package identity was not preserved",
        )));
    }
    session
        .compile_package_with_contract(&discovered.build_input, contract, identity)
        .map_err(Into::into)
}

impl generated::GameHost for EngineHost {
    fn update(
        &mut self,
        _: &mut ResourceContext<'_>,
        entity: i32,
        delta: i32,
    ) -> Result<i32, generated::HostError> {
        Ok(entity + delta)
    }

    fn animation(
        &mut self,
        context: &mut ResourceContext<'_>,
        _: i32,
    ) -> Result<nexa_runtime::HostRequestHandle, generated::HostError> {
        let pending = context
            .create_request()
            .map_err(|error| generated::HostError(error.to_string()))?;
        let request = pending.request;
        *self
            .last_request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pending);
        Ok(request)
    }

    fn action_lock(
        &mut self,
        context: &mut ResourceContext<'_>,
        _: i32,
    ) -> Result<generated::ActionLockToken, generated::HostError> {
        let token = context
            .create_token(
                generated::ActionLockToken::CONTENT_TYPE_ID,
                RuntimeHostDomain::Render,
            )
            .map_err(|error| generated::HostError(error.to_string()))?;
        *self
            .last_token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token);
        generated::ActionLockToken::try_from_raw(token)
            .map_err(|error| generated::HostError(format!("{error:?}")))
    }

    fn world_snapshot(
        &mut self,
        context: &mut ResourceContext<'_>,
    ) -> Result<generated::EnemyViewSnapshot, generated::HostError> {
        let value = generated::EnemyView { health: 10 };
        let encoded = generated::EnemyViewSnapshotEncoder::encode(&value)?;
        let snapshot = context
            .create_typed_snapshot(encoded)
            .map_err(|error| generated::HostError(error.to_string()))?;
        *self
            .last_snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot);
        Ok(generated::EnemyViewSnapshot::try_from_raw(snapshot)
            .expect("snapshot was created with the generated content type"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExpectedReleases {
    requests: usize,
    tokens: usize,
    snapshots: usize,
}

fn assert_release_batch(runtime_host: &RuntimeHost, expected: ExpectedReleases) {
    let releases = runtime_host.drain_releases();
    for (kind, expected_count) in [
        (ReleaseKind::HostRequest, expected.requests),
        (ReleaseKind::ResourceToken, expected.tokens),
        (ReleaseKind::Snapshot, expected.snapshots),
    ] {
        assert_eq!(
            releases
                .iter()
                .filter(|release| release.kind == kind)
                .count(),
            expected_count,
            "unexpected {kind:?} release count"
        );
    }
    assert_eq!(
        releases.len(),
        expected.requests + expected.tokens + expected.snapshots,
        "unexpected release kind"
    );
    assert!(runtime_host.drain_releases().is_empty());
    assert!(runtime_host.drain_releases().is_empty());
}

fn assert_terminal_resources(
    realm: &mut RealmRuntime,
    runtime_host: &RuntimeHost,
    task: nexa_runtime::TaskHandle,
    expected: ExpectedReleases,
) -> Result<(), nexa_runtime::RuntimeError> {
    realm.tick(TickBudget {
        max_tasks: 0,
        frame_fuel_budget: 0,
        collect_garbage: false,
    })?;
    assert!(realm.terminal_record(task).is_some());
    let ledger = realm.resource_ledger();
    assert_eq!(ledger.requests, 0);
    assert_eq!(ledger.tokens, 0);
    assert_eq!(ledger.snapshots, 0);
    assert_eq!(ledger.completion_reservations, 0);
    assert_eq!(ledger.release_reservations, 0);
    assert_release_batch(runtime_host, expected);
    Ok(())
}

fn assert_generated_resources(
    realm: &RealmRuntime,
    last_token: &Arc<Mutex<Option<nexa_runtime::ResourceTokenHandle>>>,
    last_snapshot: &Arc<Mutex<Option<nexa_runtime::SnapshotHandle>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    last_token
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .expect("generated action_lock returned a token");
    let snapshot = last_snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .expect("generated world_snapshot returned a typed snapshot");
    assert_eq!(
        realm.snapshot_content_type(snapshot)?,
        generated::EnemyViewSnapshotEncoder::CONTENT_TYPE
    );
    let layout = realm.snapshot_layout(snapshot)?;
    assert_eq!(
        layout.schema_hash,
        generated::EnemyViewSnapshotEncoder::SCHEMA_HASH
    );
    assert_eq!(layout.alignment, 1);
    assert_eq!(layout.size, 4);
    let decoded = realm
        .snapshot_view::<generated::EnemyViewSnapshotRef<'_>>(snapshot)?
        .decode_owned()
        .map_err(|_| HostFailure("generated snapshot payload did not decode"))?;
    assert_eq!(decoded.health, 10);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run()
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let declared_idl = nexa_contract::parse_contract(include_str!("../combat_api.contract.nexa"))?;
    assert_eq!(
        include_str!(concat!(env!("OUT_DIR"), "/combat_api.rs")),
        nexa_contract::generate_rust_for_source_file(&declared_idl, "combat_api.contract.nexa")?,
        "Combat bindings must be generated without manual edits"
    );
    assert_eq!(
        <generated::Update as nexa_runtime::ScriptExport>::NAME,
        "update"
    );
    let _typed_export_id = <generated::Update as nexa_runtime::ScriptExport>::STABLE_ID;
    let idl = nexa_contract::parse_contract(include_str!("../combat_api.contract.nexa"))?;
    assert_eq!(
        generated::CONTRACT_FINGERPRINT,
        nexa_contract::contract_fingerprint(&idl).into_bytes()
    );
    let host_contract_id = generated::CONTRACT_RUNTIME_ID;
    let host_source = Arc::<str>::from(include_str!("../combat_api.contract.nexa"));
    let host_contract = nexa::HostContractInput::with_source(
        &idl,
        nexa::SourceIdentity::standalone(COMBAT_HOST_SOURCE),
        Arc::clone(&host_source),
    )?;
    let mut build_session = nexa::PackageBuildSession::new();
    let initial = compile_combat_candidate(
        &mut build_session,
        &host_contract,
        1,
        include_str!("../gameplay.nexa"),
    )?;
    let state_schema_fingerprint = initial.module().state_schema_fingerprint;
    let combat_buffer = initial
        .module()
        .buffer_types
        .iter()
        .find(|buffer| buffer.element == nexa_bytecode::ValueType::I32)
        .copied()
        .ok_or(HostFailure("combat buffer metadata was not emitted"))?;
    let animation_checked = <generated::AnimationChecked as nexa_runtime::ScriptExport>::STABLE_ID;
    let feature_probe = <generated::FeatureProbe as nexa_runtime::ScriptExport>::STABLE_ID;
    let verified = initial.verified;
    let last_request = Arc::new(Mutex::new(None));
    let last_token = Arc::new(Mutex::new(None));
    let last_snapshot = Arc::new(Mutex::new(None));
    let runtime_host = RuntimeHost::new(4_096);
    let registry = generated::GeneratedHostRegistry::new(EngineHost {
        last_request: Arc::clone(&last_request),
        last_token: Arc::clone(&last_token),
        last_snapshot: Arc::clone(&last_snapshot),
    });
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(registry),
    )?;
    let module = realm.load_module(verified, host_contract_id, state_schema_fingerprint)?;
    let enemy_brain = StableId::from_name("boss");
    let replaced_handle = realm.insert_state(
        module,
        enemy_brain,
        nexa_runtime::StateValue::Object(nexa_runtime::StateObject {
            type_id: combat_symbol(SymbolKind::Type, "EnemyBrain"),
            version: 1,
            fields: BTreeMap::from([
                (
                    combat_symbol(SymbolKind::Field, "EnemyBrain.phase"),
                    nexa_runtime::StateValue::I32(3),
                ),
                (
                    combat_symbol(SymbolKind::Field, "EnemyBrain.legacy_target"),
                    nexa_runtime::StateValue::I32(17),
                ),
            ]),
        }),
    )?;
    let preserved_brain = StableId::from_name("preserved");
    let preserved_handle = realm.insert_state(
        module,
        preserved_brain,
        nexa_runtime::StateValue::Object(nexa_runtime::StateObject {
            type_id: combat_symbol(SymbolKind::Type, "StableBrain"),
            version: 1,
            fields: BTreeMap::from([(
                combat_symbol(SymbolKind::Field, "StableBrain.phase"),
                nexa_runtime::StateValue::I32(5),
            )]),
        }),
    )?;
    let deleted_brain = StableId::from_name("deleted");
    let deleted_handle = realm.insert_state(
        module,
        deleted_brain,
        nexa_runtime::StateValue::Object(nexa_runtime::StateObject {
            type_id: combat_symbol(SymbolKind::Type, "EnemyBrain"),
            version: 1,
            fields: BTreeMap::from([
                (
                    combat_symbol(SymbolKind::Field, "EnemyBrain.phase"),
                    nexa_runtime::StateValue::I32(9),
                ),
                (
                    combat_symbol(SymbolKind::Field, "EnemyBrain.legacy_target"),
                    nexa_runtime::StateValue::I32(23),
                ),
            ]),
        }),
    )?;
    let scope = realm.create_scope(None)?;
    let buffer = realm.allocate_buffer(
        combat_buffer.type_id,
        combat_buffer.element,
        &[
            RuntimeValue::I32(4),
            RuntimeValue::I32(5),
            RuntimeValue::I32(6),
        ],
    )?;
    let feature_task = realm.spawn_task(
        module,
        feature_probe,
        &[buffer],
        StepConfig {
            owner: scope,
            priority: 10,
            fuel_slice: 1_024,
            cumulative_budget: 4_096,
            limits: TaskLimits::default(),
        },
    )?;
    assert!(matches!(
        realm.poll_task(feature_task, 1_024)?,
        TaskPoll::Completed(RuntimeValue::I32(9))
    ));
    let checked = realm.spawn_task(
        module,
        animation_checked,
        &[RuntimeValue::I32(7)],
        StepConfig {
            owner: scope,
            priority: 10,
            fuel_slice: 64,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    assert!(matches!(
        realm.poll_task(checked, 64)?,
        TaskPoll::Waiting(_)
    ));
    let pending = last_request
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .expect("checked animation request was captured by the host");
    generated::AnimationCompletionTicket(pending.ticket).complete(Ok(7))?;
    realm.tick(TickBudget {
        max_tasks: 1,
        frame_fuel_budget: 64,
        collect_garbage: false,
    })?;
    assert!(matches!(
        realm.terminal_record(checked).map(|record| &record.reason),
        Some(nexa_runtime::TaskTerminalReason::Completed(Some(
            RuntimeValue::NamedRef { .. }
        )))
    ));
    let checked_error = realm.spawn_task(
        module,
        animation_checked,
        &[RuntimeValue::I32(8)],
        StepConfig {
            owner: scope,
            priority: 10,
            fuel_slice: 64,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    assert!(matches!(
        realm.poll_task(checked_error, 64)?,
        TaskPoll::Waiting(_)
    ));
    let pending = last_request
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .expect("failing checked animation request was captured by the host");
    generated::AnimationCompletionTicket(pending.ticket)
        .complete(Err(generated::AnimationError::EntityGone))?;
    realm.tick(TickBudget {
        max_tasks: 1,
        frame_fuel_budget: 64,
        collect_garbage: false,
    })?;
    assert!(matches!(
        realm
            .terminal_record(checked_error)
            .map(|record| &record.reason),
        Some(nexa_runtime::TaskTerminalReason::Completed(Some(
            RuntimeValue::NamedRef { .. }
        )))
    ));

    let task = realm.spawn_export::<generated::Update>(
        module,
        &generated::UpdateArgs { entity: 41 },
        StepConfig {
            owner: scope,
            priority: 10,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;

    assert!(matches!(realm.poll_task(task, 32)?, TaskPoll::Waiting(_)));
    assert_generated_resources(&realm, &last_token, &last_snapshot)?;
    let pending = last_request
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .expect("animation request was captured by the host");
    let mut ticket = generated::AnimationCompletionTicket(pending.ticket);
    ticket.complete(Ok(1))?;
    realm.tick(TickBudget {
        max_tasks: 1,
        frame_fuel_budget: 32,
        collect_garbage: true,
    })?;
    assert!(matches!(
        realm.terminal_record(task).map(|record| &record.reason),
        Some(nexa_runtime::TaskTerminalReason::Completed(Some(
            RuntimeValue::I32(42)
        )))
    ));
    assert_terminal_resources(
        &mut realm,
        &runtime_host,
        task,
        ExpectedReleases {
            requests: 3,
            tokens: 1,
            snapshots: 1,
        },
    )?;

    let cancelled = realm.spawn_export::<generated::Update>(
        module,
        &generated::UpdateArgs { entity: 12 },
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    assert!(matches!(
        realm.poll_task(cancelled, 32)?,
        TaskPoll::Waiting(_)
    ));
    assert_generated_resources(&realm, &last_token, &last_snapshot)?;
    let mut cancelled_pending = last_request
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .expect("cancelled animation request was captured by the host");
    assert!(matches!(
        realm.cancel_task(cancelled, nexa_runtime::CancelReason::HostCancelled)?,
        TaskPoll::Cancelled(nexa_runtime::CancelReason::HostCancelled)
    ));
    assert!(cancelled_pending.ticket.cancelled().is_err());
    assert_terminal_resources(
        &mut realm,
        &runtime_host,
        cancelled,
        ExpectedReleases {
            requests: 1,
            tokens: 1,
            snapshots: 1,
        },
    )?;

    let live = realm.spawn_export::<generated::Update>(
        module,
        &generated::UpdateArgs { entity: 10 },
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    assert!(matches!(realm.poll_task(live, 32)?, TaskPoll::Waiting(_)));
    assert_generated_resources(&realm, &last_token, &last_snapshot)?;
    let mut late_pending = last_request
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .expect("late animation request was captured by the host");
    let v2_artifact = compile_combat_candidate(
        &mut build_session,
        &host_contract,
        2,
        include_str!("../reload/v2.nexa"),
    )?;
    let v2 = v2_artifact.verified;
    let RestartReloadOutcome::Committed(v2) = realm.restart_reload(
        module,
        v2,
        RestartReloadPolicy {
            migration_arguments: vec![RuntimeValue::I32(10)],
            activation_arguments: vec![RuntimeValue::I32(10)],
            activation_fuel: 4_096,
        },
    )?
    else {
        return Err(Box::new(HostFailure(
            "combat restart reload did not commit",
        )));
    };
    let migrated_state = realm
        .state_handles(v2)?
        .into_iter()
        .find(|handle| handle.stable_id == enemy_brain)
        .ok_or(HostFailure("EnemyBrain state was not migrated"))?;
    assert!(realm.resolve_state(v2, replaced_handle).is_err());
    assert!(realm.resolve_state(v2, deleted_handle).is_err());
    assert_eq!(
        realm
            .state_handles(v2)?
            .into_iter()
            .find(|handle| handle.stable_id == preserved_brain),
        Some(preserved_handle)
    );
    let nexa_runtime::StateValue::Object(migrated) = realm.resolve_state(v2, migrated_state)?
    else {
        return Err(Box::new(HostFailure("EnemyBrain state is not an object")));
    };
    assert_eq!(migrated.version, 2);
    assert_eq!(
        migrated
            .fields
            .get(&combat_symbol(SymbolKind::Field, "EnemyBrain.phase")),
        Some(&nexa_runtime::StateValue::I32(3))
    );
    assert_eq!(
        migrated
            .fields
            .get(&combat_symbol(SymbolKind::Field, "EnemyBrain.aggression")),
        Some(&nexa_runtime::StateValue::I32(0))
    );
    assert!(!migrated.fields.contains_key(&combat_symbol(
        SymbolKind::Field,
        "EnemyBrain.legacy_target"
    )));
    assert!(matches!(
        realm.terminal_record(live).map(|record| &record.reason),
        Some(nexa_runtime::TaskTerminalReason::Cancelled(
            nexa_runtime::CancelReason::ReloadCommit
        ))
    ));
    assert!(
        compile_combat_candidate(
            &mut build_session,
            &host_contract,
            3,
            include_str!("../reload/invalid.nexa"),
        )
        .is_err()
    );

    let rollback_task = realm.spawn_export::<generated::Update>(
        v2,
        &generated::UpdateArgs { entity: 4 },
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    let failing_source = include_str!("../reload/activation_fault.nexa").replace(
        "finish_migration();\n    return value;",
        "let failure: i32 = 1 / 0;\n    finish_migration();\n    return value + failure;",
    );
    let failing =
        compile_combat_candidate(&mut build_session, &host_contract, 4, &failing_source)?.verified;
    assert!(matches!(
        realm.restart_reload(
            v2,
            failing,
            RestartReloadPolicy {
                migration_arguments: vec![RuntimeValue::I32(1)],
                activation_arguments: vec![RuntimeValue::I32(1)],
                activation_fuel: 4_096,
            },
        )?,
        RestartReloadOutcome::RolledBackBeforeCommit { .. }
    ));
    assert_eq!(realm.active_root(), Some(v2));
    assert!(matches!(
        realm
            .terminal_record(rollback_task)
            .map(|record| &record.reason),
        Some(nexa_runtime::TaskTerminalReason::Cancelled(
            nexa_runtime::CancelReason::ReloadCommit
        ))
    ));

    let cancelled_scope = realm.create_scope(None)?;
    let cancelled_task = realm.spawn_export::<generated::Update>(
        v2,
        &generated::UpdateArgs { entity: 5 },
        StepConfig {
            owner: cancelled_scope,
            priority: 2,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    assert!(matches!(
        realm.poll_task(cancelled_task, 0)?,
        TaskPoll::Yielded(_)
    ));
    assert_eq!(realm.cancel_scope(cancelled_scope)?, 1);
    assert!(matches!(
        realm
            .terminal_record(cancelled_task)
            .map(|record| &record.reason),
        Some(nexa_runtime::TaskTerminalReason::Cancelled(
            nexa_runtime::CancelReason::ScopeCancelled
        ))
    ));

    let live = realm.spawn_export::<generated::Update>(
        v2,
        &generated::UpdateArgs { entity: 1 },
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    let fault = compile_combat_candidate(
        &mut build_session,
        &host_contract,
        5,
        include_str!("../reload/activation_fault.nexa"),
    )?
    .verified;
    let activation_probe = realm
        .failure_injector()
        .arm_once(RuntimeFailurePoint::ActivationTrap);
    let RestartReloadOutcome::ActivationFaulted {
        candidate: fault, ..
    } = realm.restart_reload(
        v2,
        fault,
        RestartReloadPolicy {
            migration_arguments: vec![RuntimeValue::I32(1)],
            activation_arguments: vec![RuntimeValue::I32(1)],
            activation_fuel: 4_096,
        },
    )?
    else {
        return Err(Box::new(HostFailure(
            "combat activation fault was not observable",
        )));
    };
    activation_probe.require_consumed().map_err(HostFailure)?;
    assert_eq!(realm.active_root(), Some(v2));
    assert_eq!(realm.module_lifecycle(v2)?, ModuleLifecycle::Active);
    assert!(
        realm.module_lifecycle(fault).is_err(),
        "the released activation-fault Candidate remained addressable"
    );
    let recovered = realm.spawn_export::<generated::Update>(
        v2,
        &generated::UpdateArgs { entity: 1 },
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    assert!(matches!(
        realm.cancel_task(recovered, nexa_runtime::CancelReason::HostCancelled)?,
        TaskPoll::Cancelled(nexa_runtime::CancelReason::HostCancelled)
    ));
    assert!(matches!(
        realm.terminal_record(live).map(|record| &record.reason),
        Some(nexa_runtime::TaskTerminalReason::Cancelled(
            nexa_runtime::CancelReason::ReloadCommit
        ))
    ));
    let discarded_before = realm.discarded_late_host_results();
    late_pending.ticket.complete(HostPayload::I32(99))?;
    realm.tick(TickBudget {
        max_tasks: 0,
        frame_fuel_budget: 0,
        collect_garbage: false,
    })?;
    assert_eq!(realm.discarded_late_host_results(), discarded_before + 1);
    drop(realm);
    assert_release_batch(
        &runtime_host,
        ExpectedReleases {
            requests: 1,
            tokens: 1,
            snapshots: 1,
        },
    );
    let _ = runtime_host.begin_close();
    runtime_host.try_finish_close()?;
    println!("combat-runtime completed with deterministic reload activation fault");
    Ok(())
}

#[derive(Debug)]
struct HostFailure(&'static str);

impl std::fmt::Display for HostFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for HostFailure {}

#[cfg(test)]
mod tests {
    #[test]
    fn generated_host_binding_releases_request_token_and_snapshot_exactly_once() {
        super::run().expect("Combat generated Host Binding lifecycle");
    }
}
