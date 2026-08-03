use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, ScriptExport, Signature, ValueType,
};
use nexa_core::{StableId, StateSchemaFingerprint};
use nexa_runtime::{
    HostCallOutcome, HostFunctionSlot, HostRegistry, HostTrap, ModuleHandle, RealmConfig,
    RealmRuntime, ResolvedHostFunction, ResourceContext, RuntimeHost, RuntimeHostArgs,
    RuntimeValue, ScopeHandle, StepConfig, TaskHandle, TaskLimits, TaskPoll,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

pub const HOST_NAME: &str = "baseline-host";
pub const BASELINE_ENTRY_EXPORT: StableId = StableId(0x5254_4241_5345_454e);
#[must_use]
pub fn hashes() -> (StableId, StateSchemaFingerprint) {
    (
        StableId::from_name(HOST_NAME),
        nexa_bytecode::StateSchema::default().fingerprint(),
    )
}

#[must_use]
pub fn verified(functions: Vec<nexa_bytecode::Function>) -> VerifiedModule {
    let (host, schema) = hashes();
    let mut module = ModuleBuilder::new();
    module.metadata(host, schema);
    for (position, function) in functions.into_iter().enumerate() {
        let signature = function.signature.clone();
        let effect = function.effect;
        let function = module.function(function);
        if position == 0 {
            module.script_export(ScriptExport {
                stable_id: BASELINE_ENTRY_EXPORT,
                function,
                signature,
                effect,
            });
        }
    }
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

pub fn realm_with(
    code: impl IntoIterator<Item = Instruction>,
) -> (
    RealmRuntime,
    nexa_runtime::ModuleHandle,
    StableId,
    StateSchemaFingerprint,
) {
    let (host, schema) = hashes();
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        2,
    );
    function.effect(FunctionEffect::Task);
    for instruction in code {
        function.emit(instruction);
    }
    let verified = verified(vec![function.finish().unwrap()]);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        RuntimeHost::new(64),
        Box::new(NoHost(host)),
    )
    .unwrap();
    let module = realm.load_module(verified, host, schema).unwrap();
    (realm, module, host, schema)
}

struct NoHost(StableId);

impl HostRegistry for NoHost {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn resolve_function(&self, _: StableId) -> Option<ResolvedHostFunction<'_>> {
        None
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        _: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::InvalidFunctionSlot(slot))
    }
}

#[must_use]
pub fn task_config(owner: ScopeHandle) -> StepConfig {
    StepConfig {
        owner,
        priority: 1,
        fuel_slice: 16,
        cumulative_budget: 128,
        limits: TaskLimits::default(),
    }
}

pub fn spawn_test_task(
    realm: &mut RealmRuntime,
    module: ModuleHandle,
) -> (ScopeHandle, TaskHandle) {
    let scope = realm.create_scope(None).unwrap();
    let task = realm
        .spawn_task(
            module,
            BASELINE_ENTRY_EXPORT,
            &[RuntimeValue::I32(7)],
            task_config(scope),
        )
        .unwrap();
    (scope, task)
}

#[must_use]
pub fn snapshot(
    realm: &RealmRuntime,
    scope: ScopeHandle,
    task: TaskHandle,
    result: &TaskPoll,
    extra: &str,
) -> String {
    format!(
        "result={result:?}\nscope_state={:?}\nterminal_state={:?}\nterminal_reason={:?}\nepoch={:?}\ncharge={:?}\ntrace={:?}\n{extra}",
        realm.scope_snapshot(scope).unwrap().state,
        realm.terminal_record(task).map(|record| record.state),
        realm.terminal_record(task).map(|record| &record.reason),
        realm
            .terminal_record(task)
            .map(|record| record.module_epoch),
        realm
            .terminal_record(task)
            .map(|record| record.final_charge),
        realm
            .trace()
            .records()
            .iter()
            .map(|record| (record.sequence, record.transition_id.0))
            .collect::<Vec<_>>()
    )
}

pub fn assert_snapshot(name: &str, actual: &str, expected: &str) {
    if actual != expected {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/runtime-traces");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(format!("{name}.actual.snap")), actual).unwrap();
    }
    assert_eq!(actual, expected);
}
