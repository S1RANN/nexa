use std::sync::Arc;

use nexa_core::StableId;
use nexa_runtime::{
    HostArgs, HostCallOutcome, HostCompletion, HostPayload, HostRegistry, HostTrap, HostValue,
    PollResult, RealmConfig, RealmRuntime, ResourceContext, RuntimeHostDomain, RuntimeValue,
    ScriptFunction, StepConfig, TaskLimits, TickBudget,
};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/engine.rs"));
}

struct EngineHost;

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
        context
            .create_request()
            .map_err(|error| generated::HostError(error.to_string()))
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
    let verified = nexa_compiler::compile_with_metadata(
        include_str!("../gameplay.nexa"),
        host_hash,
        schema_hash,
    )?;
    let mut realm = RealmRuntime::new(RealmConfig::default());
    let module = realm.load_module(verified, host_hash, schema_hash)?;
    let enemy_brain = StableId::from_name("EnemyBrain::boss");
    realm.insert_state(module, enemy_brain, nexa_runtime::StateValue::I32(3))?;
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

    let mut host = generated::GeneratedHostRegistry::new(EngineHost);
    let update = realm
        .with_resource_context(task, |context| {
            host.call(
                generated::THUNK_UPDATE,
                context,
                HostArgs::new(&[HostValue::I32(40), HostValue::I32(2)]),
            )
        })?
        .map_err(HostFailure::from)?;
    assert_eq!(update, HostCallOutcome::Immediate(HostValue::I32(42)));
    let request = realm
        .with_resource_context(task, |context| {
            host.call(
                generated::THUNK_ANIMATION,
                context,
                HostArgs::new(&[HostValue::I32(7)]),
            )
        })?
        .map_err(HostFailure::from)?;
    let HostCallOutcome::Pending(request) = request else {
        return Err(Box::new(HostFailure("animation did not return a request")));
    };
    realm
        .with_resource_context(task, |context| {
            host.call(
                generated::THUNK_ACTION_LOCK,
                context,
                HostArgs::new(&[HostValue::I32(7)]),
            )
        })?
        .map_err(HostFailure::from)?;
    let snapshot = realm
        .with_resource_context(task, |context| {
            host.call(generated::THUNK_WORLD_SNAPSHOT, context, HostArgs::new(&[]))
        })?
        .map_err(HostFailure::from)?;
    let HostCallOutcome::Immediate(HostValue::Snapshot(snapshot)) = snapshot else {
        return Err(Box::new(HostFailure("snapshot thunk returned wrong type")));
    };
    assert_eq!(realm.snapshot_data(snapshot)?, &[10, 20, 30]);

    assert!(matches!(realm.poll_task(task, 32)?, PollResult::Pending(_)));
    realm.wait_for_request(task, request)?;
    realm.completion_sender().complete(HostCompletion {
        realm_id: realm.realm_id(),
        module_id: module.raw().index,
        epoch: realm.module_epoch(module)?,
        request,
        payload: HostPayload::I32(1),
    })?;
    realm.tick(TickBudget {
        max_tasks: 1,
        frame_fuel_budget: 32,
        collect_garbage: true,
    })?;

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
    let late_request = realm.create_host_request(live)?;
    realm.wait_for_request(live, late_request)?;
    let late_sender = realm.completion_sender();
    let old_epoch = realm.module_epoch(module)?;
    let v2 = nexa_compiler::compile_with_metadata(
        include_str!("../reload/v2.nexa"),
        host_hash,
        schema_hash,
    )?;
    let v2 = realm.prepare_reload(module, v2, host_hash, schema_hash)?;
    realm.quiesce_reload()?;
    assert_eq!(
        realm.stage_reload(0, &[RuntimeValue::I32(10)])?,
        Some(RuntimeValue::I32(10))
    );
    realm.commit_reload(|_| Ok(()))?;
    let migrated_state = realm
        .state_handles(v2)?
        .into_iter()
        .find(|handle| handle.stable_id == enemy_brain)
        .ok_or(HostFailure("EnemyBrain state was not migrated"))?;
    assert_eq!(
        realm.resolve_state(v2, migrated_state)?,
        &nexa_runtime::StateValue::I32(3)
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

    late_sender.complete(HostCompletion {
        realm_id: realm.realm_id(),
        module_id: module.raw().index,
        epoch: old_epoch,
        request: late_request,
        payload: HostPayload::I32(99),
    })?;
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
        realm.poll_task(cancelled_task, 32)?,
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
        schema_hash,
    )?;
    let fault = realm.prepare_reload(v2, fault, host_hash, schema_hash)?;
    realm.quiesce_reload()?;
    realm.stage_reload(0, &[RuntimeValue::I32(1)])?;
    assert!(
        realm
            .commit_reload(|_| Err("activation fault".into()))
            .is_err()
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
    assert!(realm.terminal_record(live).is_some());
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

impl From<HostTrap> for HostFailure {
    fn from(_: HostTrap) -> Self {
        Self("host trap")
    }
}
