#![allow(deprecated)]

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    HostArgs, HostCallOutcome, HostRegistry, HostTrap, ModuleHandle, PollResult, RealmConfig,
    RealmRuntime, ResourceContext, RuntimeHost, RuntimeValue, ScopeHandle, StepConfig, TaskHandle,
    TaskLimits,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

pub const HOST_NAME: &str = "baseline-host";
pub const SCHEMA_NAME: &str = "baseline-schema";

#[must_use]
pub fn hashes() -> (StableId, StableId) {
    (
        StableId::from_name(HOST_NAME),
        StableId::from_name(SCHEMA_NAME),
    )
}

#[must_use]
pub fn verified(functions: Vec<nexa_bytecode::Function>) -> VerifiedModule {
    let (host, schema) = hashes();
    let mut module = ModuleBuilder::new();
    module.metadata(host, schema);
    for function in functions {
        module.function(function);
    }
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

pub fn realm_with(
    code: impl IntoIterator<Item = Instruction>,
) -> (RealmRuntime, nexa_runtime::ModuleHandle, StableId, StableId) {
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

pub fn spawn(realm: &mut RealmRuntime, module: ModuleHandle) -> (ScopeHandle, TaskHandle) {
    let scope = realm.create_scope(None).unwrap();
    let task = realm
        .call(module, 0, &[RuntimeValue::I32(7)], task_config(scope))
        .unwrap();
    (scope, task)
}

#[must_use]
pub fn snapshot(
    realm: &RealmRuntime,
    scope: ScopeHandle,
    task: TaskHandle,
    result: &PollResult<Option<RuntimeValue>>,
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
