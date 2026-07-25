use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_model::system::{
    RealmSystemConfig, RealmSystemEvent, RealmSystemSnapshot, replay_realm_runtime,
};
use nexa_runtime::{
    ActivationEntry, CancelReason, HostArgs, HostCallOutcome, HostPayload, HostRegistry, HostTrap,
    ModuleLifecycle, RealmConfig, RealmRuntime, ResourceContext, RuntimeHost, RuntimeHostDomain,
    RuntimeValue, StepConfig, TaskLimits, TaskTerminalReason, TickBudget,
};
use nexa_verifier::{VerifierLimits, verify};

#[test]
#[allow(clippy::too_many_lines)]
fn realm_resource_sequence_matches_composite_reference_model() {
    let config = RealmSystemConfig {
        max_depth: 8,
        max_requests: 1,
        max_tokens: 1,
    };
    let mut model = RealmSystemSnapshot::default();
    let host_hash = StableId::from_name("realm-differential-host");
    let schema_hash = StableId::from_name("realm-differential-schema");
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module
        .metadata(host_hash, schema_hash)
        .function(function.finish().unwrap());
    let module = verify(module.finish(), VerifierLimits::default()).unwrap();

    let host = RuntimeHost::new(16);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        host.clone(),
        Box::new(NoHost(host_hash)),
    )
    .unwrap();
    let module = realm.load_module(module, host_hash, schema_hash).unwrap();
    let scope = realm.create_scope(None).unwrap();
    let task = realm
        .call(
            module,
            0,
            &[RuntimeValue::I32(7)],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 64,
                cumulative_budget: 1_024,
                limits: TaskLimits::default(),
            },
        )
        .unwrap();
    let mut pending = realm.create_host_request(task).unwrap();
    realm.wait_for_request(task, pending.request).unwrap();
    realm
        .create_resource_token(task, RuntimeHostDomain::Render)
        .unwrap();
    model
        .apply(RealmSystemEvent::SubmitRequest, config)
        .unwrap();
    model.apply(RealmSystemEvent::AcquireToken, config).unwrap();
    assert_runtime_resources(&realm, host.pending_releases(), model);

    pending.ticket.complete(HostPayload::I32(9)).unwrap();
    realm
        .tick(TickBudget {
            max_tasks: 0,
            frame_fuel_budget: 0,
            collect_garbage: false,
        })
        .unwrap();
    model
        .apply(RealmSystemEvent::CompleteRequest, config)
        .unwrap();
    assert_runtime_resources(&realm, host.pending_releases(), model);

    realm.poll_task(task, 64).unwrap();
    realm
        .tick(TickBudget {
            max_tasks: 0,
            frame_fuel_budget: 0,
            collect_garbage: false,
        })
        .unwrap();
    model.apply(RealmSystemEvent::ReleaseToken, config).unwrap();
    assert_runtime_resources(&realm, host.pending_releases(), model);

    assert_eq!(host.drain_releases().len(), 2);
    model
        .apply(RealmSystemEvent::DrainReleases, config)
        .unwrap();
    assert_runtime_resources(&realm, host.pending_releases(), model);
    assert_eq!(
        replay_realm_runtime(
            config,
            [
                RealmSystemEvent::SubmitRequest,
                RealmSystemEvent::AcquireToken,
                RealmSystemEvent::CompleteRequest,
                RealmSystemEvent::ReleaseToken,
                RealmSystemEvent::DrainReleases,
            ],
        )
        .unwrap(),
        model
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn activation_failure_after_publication_matches_irreversible_reload_model() {
    let config = RealmSystemConfig {
        max_depth: 8,
        max_requests: 1,
        max_tokens: 1,
    };
    let host_hash = StableId::from_name("realm-reload-differential-host");
    let schema_hash = StableId::from_name("realm-reload-differential-schema");

    let mut old_task = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    old_task
        .effect(FunctionEffect::Task)
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let old_module = verified_module(host_hash, schema_hash, vec![old_task.finish().unwrap()]);

    let mut migration = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    migration
        .effect(FunctionEffect::Migration)
        .emit(Instruction::Return { source: 0 });
    let mut activation = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    activation
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::Trap);
    let candidate_module = verified_module(
        host_hash,
        schema_hash,
        vec![migration.finish().unwrap(), activation.finish().unwrap()],
    );

    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let old = realm
        .load_module(old_module, host_hash, schema_hash)
        .unwrap();
    let scope = realm.create_scope(None).unwrap();
    let task = realm
        .call(
            old,
            0,
            &[RuntimeValue::I32(7)],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 64,
                cumulative_budget: 1_024,
                limits: TaskLimits::default(),
            },
        )
        .unwrap();
    let candidate = realm
        .prepare_reload(old, candidate_module, host_hash, schema_hash)
        .unwrap();
    realm.quiesce_reload().unwrap();
    realm.stage_reload(0, &[RuntimeValue::I32(7)]).unwrap();
    assert!(
        realm
            .commit_reload(ActivationEntry {
                function_id: 1,
                arguments: &[],
                fuel: 64,
            })
            .is_err()
    );

    let model = replay_realm_runtime(
        config,
        [
            RealmSystemEvent::StartOldTask,
            RealmSystemEvent::BeginReload,
            RealmSystemEvent::PublishReload,
            RealmSystemEvent::BeginActivation,
            RealmSystemEvent::ActivationFailed,
        ],
    )
    .unwrap();
    assert_eq!(model.active_epoch, 1);
    assert!(!model.old_task_live);
    assert_eq!(
        realm.module_lifecycle(old).unwrap(),
        ModuleLifecycle::Retired
    );
    assert_eq!(
        realm.module_lifecycle(candidate).unwrap(),
        ModuleLifecycle::ActivationFaulted
    );
    assert_eq!(realm.active_root(), Some(candidate));
    assert!(matches!(
        realm.terminal_record(task).map(|record| &record.reason),
        Some(TaskTerminalReason::Cancelled(CancelReason::ReloadCommit))
    ));
    assert_eq!(realm.root_publications().len(), 1);
}

fn verified_module(
    host_hash: StableId,
    schema_hash: StableId,
    functions: Vec<nexa_bytecode::Function>,
) -> nexa_verifier::VerifiedModule {
    let mut module = ModuleBuilder::new();
    module.metadata(host_hash, schema_hash);
    for function in functions {
        module.function(function);
    }
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

struct NoHost(StableId);

impl HostRegistry for NoHost {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        _: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::UnknownFunction(id))
    }
}

fn assert_runtime_resources(
    realm: &RealmRuntime,
    host_releases: usize,
    model: RealmSystemSnapshot,
) {
    let runtime = realm.resource_snapshot();
    assert_eq!(runtime.requests, model.requests);
    assert_eq!(runtime.tokens, model.tokens);
    assert_eq!(runtime.release_reservations, model.reservations);
    assert_eq!(runtime.release_records + host_releases, model.releases);
}
