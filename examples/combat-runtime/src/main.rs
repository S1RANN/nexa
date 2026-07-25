use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use nexa_bytecode::{Instruction, RootMap, ValueType};
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
    let enemy_brain = StableId::from_name("EnemyBrain::boss");
    realm.insert_state(
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
    let scope = realm.create_scope(None)?;
    let task = realm.call(
        module,
        1,
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
        1,
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
    let mut v2 = v2.into_module();
    let migration = &mut v2.functions[0];
    let phase = StableId::from_name("EnemyBrain::phase");
    let aggression = StableId::from_name("EnemyBrain::aggression");
    let brain_type = StableId::from_name("EnemyBrain");
    migration.code = vec![
        Instruction::StateOldGet {
            stable_id: enemy_brain,
            ty: ValueType::Named(brain_type),
            dst: 1,
        },
        Instruction::StateOldFieldGet {
            object: 1,
            field_id: phase,
            ty: ValueType::I32,
            dst: 2,
        },
        Instruction::LoadI32 { dst: 3, value: 0 },
        Instruction::StateNewCreate {
            stable_id: enemy_brain,
            type_id: brain_type,
            dst: 4,
        },
        Instruction::StateNewSet {
            object: 4,
            field_id: phase,
            source: 2,
        },
        Instruction::StateNewSet {
            object: 4,
            field_id: aggression,
            source: 3,
        },
        Instruction::StateReplace {
            old_id: enemy_brain,
            target: 4,
        },
        Instruction::StateFinish,
        Instruction::Return { source: 0 },
    ];
    migration.root_bitmap.fill(false);
    migration.root_bitmap[1] = true;
    migration.root_bitmap[4] = true;
    migration.safepoints = vec![0, 8];
    let mut terminal_roots = vec![false; usize::from(migration.registers)];
    terminal_roots[1] = true;
    terminal_roots[4] = true;
    migration.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false; usize::from(migration.registers)],
        },
        RootMap {
            pc: 8,
            bitmap: terminal_roots,
        },
    ];
    let v2 = nexa_verifier::verify(v2, nexa_verifier::VerifierLimits::default())?;
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
