use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_model::system::{
    RealmSystemConfig, RealmSystemEvent, RealmSystemSnapshot, replay_realm_runtime,
};
use nexa_runtime::{
    HostArgs, HostCallOutcome, HostCompletion, HostPayload, HostRegistry, HostTrap, RealmConfig,
    RealmRuntime, ResourceContext, RuntimeHost, RuntimeHostDomain, RuntimeValue, StepConfig,
    TaskLimits, TickBudget,
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
    let mut realm =
        RealmRuntime::with_runtime_host(RealmConfig::default(), host.clone(), Box::new(NoHost));
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
    let request = realm.create_host_request(task).unwrap();
    realm.wait_for_request(task, request).unwrap();
    realm
        .create_resource_token(task, RuntimeHostDomain::Render)
        .unwrap();
    model
        .apply(RealmSystemEvent::SubmitRequest, config)
        .unwrap();
    model.apply(RealmSystemEvent::AcquireToken, config).unwrap();
    assert_runtime_resources(&realm, host.pending_releases(), model);

    realm
        .completion_sender()
        .complete(HostCompletion {
            realm_id: realm.realm_id(),
            module_id: module.raw().index,
            epoch: realm.module_epoch(module).unwrap(),
            request,
            payload: HostPayload::I32(9),
        })
        .unwrap();
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

struct NoHost;

impl HostRegistry for NoHost {
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
