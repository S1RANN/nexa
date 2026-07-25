use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use nexa_core::StableId;
use nexa_runtime::{
    ActivationEntry, HostPayload, ModuleLifecycle, PollResult, RealmConfig, RealmRuntime,
    ResourceContext, RuntimeHost, RuntimeHostDomain, RuntimeValue, ScriptFunction, StepConfig,
    TaskLimits, TickBudget,
};

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/engine.rs"));
}

struct EngineHost {
    last_request: Arc<Mutex<Option<nexa_runtime::PendingHostRequest>>>,
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
    ) -> Result<nexa_runtime::ResourceTokenHandle, generated::HostError> {
        context
            .create_token(RuntimeHostDomain::Render)
            .map_err(|error| generated::HostError(error.to_string()))
    }

    fn world_snapshot(
        &mut self,
        context: &mut ResourceContext<'_>,
    ) -> Result<nexa_runtime::SnapshotHandle, generated::HostError> {
        context
            .create_snapshot(Arc::from([10, 20, 30]))
            .map_err(|error| generated::HostError(error.to_string()))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(generated::Update::EXPORT_NAME, "Update");
    let _typed_export_id = generated::Update::FUNCTION_ID;
    let idl = nexa_idl::parse(include_str!("../engine.idl"))?;
    let host_hash = nexa_idl::exact_hash(&idl);
    let schema_hash = StableId::from_name("combat-state-v1");
    let schema_hash_v2 = StableId::from_name("combat-state-v2");
    let verified =
        nexa_compiler::compile_with_interface(include_str!("../gameplay.nexa"), &idl, schema_hash)?;
    let last_request = Arc::new(Mutex::new(None));
    let runtime_host = RuntimeHost::new(4_096);
    let registry = generated::GeneratedHostRegistry::new(EngineHost {
        last_request: Arc::clone(&last_request),
    });
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(registry),
    )?;
    let module = realm.load_module(verified, host_hash, schema_hash)?;
    let enemy_brain = StableId::from_name("boss");
    let replaced_handle = realm.insert_state(
        module,
        enemy_brain,
        nexa_runtime::StateValue::Object(nexa_runtime::StateObject {
            type_id: StableId::from_name("EnemyBrain"),
            version: 1,
            fields: BTreeMap::from([
                (
                    StableId::from_name("EnemyBrain::phase"),
                    nexa_runtime::StateValue::I32(3),
                ),
                (
                    StableId::from_name("EnemyBrain::legacy_target"),
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
            type_id: StableId::from_name("StableBrain"),
            version: 1,
            fields: BTreeMap::from([(
                StableId::from_name("StableBrain::phase"),
                nexa_runtime::StateValue::I32(5),
            )]),
        }),
    )?;
    let deleted_brain = StableId::from_name("deleted");
    let deleted_handle = realm.insert_state(
        module,
        deleted_brain,
        nexa_runtime::StateValue::Object(nexa_runtime::StateObject {
            type_id: StableId::from_name("EnemyBrain"),
            version: 1,
            fields: BTreeMap::from([
                (
                    StableId::from_name("EnemyBrain::phase"),
                    nexa_runtime::StateValue::I32(9),
                ),
                (
                    StableId::from_name("EnemyBrain::legacy_target"),
                    nexa_runtime::StateValue::I32(23),
                ),
            ]),
        }),
    )?;
    let scope = realm.create_scope(None)?;
    let checked = realm.call(
        module,
        3,
        &[RuntimeValue::I32(7)],
        StepConfig {
            owner: scope,
            priority: 10,
            fuel_slice: 64,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    assert_eq!(
        realm.poll_task(checked, 64)?,
        PollResult::Pending(nexa_runtime::PendingReason::HostRequest)
    );
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
    let checked_error = realm.call(
        module,
        3,
        &[RuntimeValue::I32(8)],
        StepConfig {
            owner: scope,
            priority: 10,
            fuel_slice: 64,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    assert_eq!(
        realm.poll_task(checked_error, 64)?,
        PollResult::Pending(nexa_runtime::PendingReason::HostRequest)
    );
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

    let task = realm.call(
        module,
        4,
        &[RuntimeValue::I32(41)],
        StepConfig {
            owner: scope,
            priority: 10,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;

    assert_eq!(
        realm.poll_task(task, 32)?,
        PollResult::Pending(nexa_runtime::PendingReason::HostRequest)
    );
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

    let live = realm.call(
        module,
        4,
        &[RuntimeValue::I32(10)],
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    let mut late_pending = realm.create_host_request(live)?;
    realm.wait_for_request(live, late_pending.request)?;
    let v2 = nexa_compiler::compile_with_metadata(
        include_str!("../reload/v2.nexa"),
        host_hash,
        schema_hash_v2,
    )?;
    let v2 = realm.prepare_reload_migrating(module, v2, host_hash)?;
    realm.quiesce_reload()?;
    assert_eq!(
        realm.stage_reload(0, &[RuntimeValue::I32(10)])?,
        Some(RuntimeValue::I32(10))
    );
    realm.commit_reload(ActivationEntry {
        function_id: 3,
        arguments: &[RuntimeValue::I32(10)],
        fuel: 4_096,
    })?;
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
            .get(&StableId::from_name("EnemyBrain::phase")),
        Some(&nexa_runtime::StateValue::I32(3))
    );
    assert_eq!(
        migrated
            .fields
            .get(&StableId::from_name("EnemyBrain::aggression")),
        Some(&nexa_runtime::StateValue::I32(0))
    );
    assert!(
        !migrated
            .fields
            .contains_key(&StableId::from_name("EnemyBrain::legacy_target"))
    );
    assert!(matches!(
        realm.terminal_record(live).map(|record| &record.reason),
        Some(nexa_runtime::TaskTerminalReason::Cancelled(
            nexa_runtime::CancelReason::ReloadCommit
        ))
    ));
    assert!(
        nexa_compiler::compile_with_metadata(
            include_str!("../reload/invalid.nexa"),
            host_hash,
            schema_hash
        )
        .is_err()
    );

    late_pending.ticket.complete(HostPayload::I32(99))?;
    realm.tick(TickBudget {
        max_tasks: 0,
        frame_fuel_budget: 0,
        collect_garbage: false,
    })?;
    assert_eq!(realm.discarded_late_host_results(), 1);

    let cancelled_scope = realm.create_scope(None)?;
    let cancelled_task = realm.call(
        v2,
        2,
        &[RuntimeValue::I32(5)],
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
        PollResult::Pending(_)
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

    let live = realm.call(
        v2,
        2,
        &[RuntimeValue::I32(1)],
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 32,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    let fault = nexa_compiler::compile_with_metadata(
        include_str!("../reload/activation_fault.nexa"),
        host_hash,
        schema_hash_v2,
    )?;
    let fault = realm.prepare_reload(v2, fault, host_hash, schema_hash_v2)?;
    realm.quiesce_reload()?;
    realm.stage_reload(0, &[RuntimeValue::I32(1)])?;
    assert!(
        realm
            .commit_reload(ActivationEntry {
                function_id: u32::MAX,
                arguments: &[],
                fuel: 4_096,
            })
            .is_err()
    );
    assert_eq!(realm.active_root(), Some(fault));
    assert_eq!(
        realm.module_lifecycle(fault)?,
        ModuleLifecycle::ActivationFaulted
    );
    assert!(
        realm
            .call(
                fault,
                0,
                &[RuntimeValue::I32(1)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 32,
                    cumulative_budget: 1_024,
                    limits: TaskLimits::default(),
                },
            )
            .is_err()
    );
    assert!(matches!(
        realm.terminal_record(live).map(|record| &record.reason),
        Some(nexa_runtime::TaskTerminalReason::Cancelled(
            nexa_runtime::CancelReason::ReloadCommit
        ))
    ));
    drop(realm);
    let _releases = runtime_host.drain_releases();
    runtime_host.begin_close();
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
