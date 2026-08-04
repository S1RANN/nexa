//! WP89 gate: `call_export_immediate` settles `@immediate` exports with
//! no Task, scheduler token, or tombstone - the resource ledger stays
//! flat across steady-state calls while fuel, traps, results, and the
//! charge match the metered Task path exactly. Non-immediate exports are
//! rejected before any lifecycle work.

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, ScriptExport as ExportDeclaration,
    Signature, StateSchema, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    MustCompletePolicy, RealmConfig, RealmRuntime, RuntimeValue, ScriptArgumentRequirements,
    ScriptArguments, ScriptCallError, ScriptCallWriter, ScriptExport, ScriptOutputReader,
    ScriptSignature,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST: StableId = StableId(0x494d_4d45_4448_4f53);
const IMMEDIATE_EXPORT: StableId = StableId(0x494d_4d45_4443_414c);
const TASK_EXPORT: StableId = StableId(0x494d_4d45_4454_4153);
const TRAP_EXPORT: StableId = StableId(0x494d_4d45_4454_5250);

struct ImmediateAdd;

impl ScriptExport for ImmediateAdd {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = IMMEDIATE_EXPORT;
    const NAME: &'static str = "immediate_add";
    const SIGNATURE: ScriptSignature =
        ScriptSignature::new(&[ValueType::I32], Some(ValueType::I32));
    const EFFECT: FunctionEffect = FunctionEffect::Immediate;

    fn argument_requirements(
        _: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Ok(ScriptArgumentRequirements::ZERO)
    }

    fn encode_args(
        _: &mut ScriptCallWriter<'_>,
        args: &Self::Args,
    ) -> Result<ScriptArguments, ScriptCallError> {
        ScriptArguments::try_from_array([RuntimeValue::I32(*args)])
    }

    fn decode_output(
        reader: &ScriptOutputReader<'_>,
        value: RuntimeValue,
    ) -> Result<Self::Output, ScriptCallError> {
        reader
            .value(value)
            .i32()
            .map_err(|_| ScriptCallError::OutputDecoding)
    }
}

struct ImmediateTrap;

impl ScriptExport for ImmediateTrap {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = TRAP_EXPORT;
    const NAME: &'static str = "immediate_trap";
    const SIGNATURE: ScriptSignature =
        ScriptSignature::new(&[ValueType::I32], Some(ValueType::I32));
    const EFFECT: FunctionEffect = FunctionEffect::Immediate;

    fn argument_requirements(
        _: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Ok(ScriptArgumentRequirements::ZERO)
    }

    fn encode_args(
        _: &mut ScriptCallWriter<'_>,
        args: &Self::Args,
    ) -> Result<ScriptArguments, ScriptCallError> {
        ScriptArguments::try_from_array([RuntimeValue::I32(*args)])
    }

    fn decode_output(
        reader: &ScriptOutputReader<'_>,
        value: RuntimeValue,
    ) -> Result<Self::Output, ScriptCallError> {
        reader
            .value(value)
            .i32()
            .map_err(|_| ScriptCallError::OutputDecoding)
    }
}

struct TaskAdd;

impl ScriptExport for TaskAdd {
    type Args = i32;
    type Output = i32;

    const STABLE_ID: StableId = TASK_EXPORT;
    const NAME: &'static str = "task_add";
    const SIGNATURE: ScriptSignature =
        ScriptSignature::new(&[ValueType::I32], Some(ValueType::I32));
    const EFFECT: FunctionEffect = FunctionEffect::Task;

    fn argument_requirements(
        _: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError> {
        Ok(ScriptArgumentRequirements::ZERO)
    }

    fn encode_args(
        _: &mut ScriptCallWriter<'_>,
        args: &Self::Args,
    ) -> Result<ScriptArguments, ScriptCallError> {
        ScriptArguments::try_from_array([RuntimeValue::I32(*args)])
    }

    fn decode_output(
        reader: &ScriptOutputReader<'_>,
        value: RuntimeValue,
    ) -> Result<Self::Output, ScriptCallError> {
        reader
            .value(value)
            .i32()
            .map_err(|_| ScriptCallError::OutputDecoding)
    }
}

fn immediate_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut immediate = FunctionBuilder::new(signature.clone(), 3);
    immediate
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::LoadI32 { dst: 1, value: 1 })
        .emit(Instruction::Add {
            dst: 2,
            lhs: 0,
            rhs: 1,
        })
        .emit(Instruction::Return { source: 2 });
    let mut task = FunctionBuilder::new(signature.clone(), 3);
    task.effect(FunctionEffect::Task)
        .emit(Instruction::LoadI32 { dst: 1, value: 1 })
        .emit(Instruction::Add {
            dst: 2,
            lhs: 0,
            rhs: 1,
        })
        .emit(Instruction::Return { source: 2 });
    let mut trapping = FunctionBuilder::new(signature.clone(), 3);
    trapping
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::LoadI32 { dst: 1, value: 0 })
        .emit(Instruction::Div {
            dst: 2,
            lhs: 0,
            rhs: 1,
        })
        .emit(Instruction::Return { source: 2 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, StateSchema::default().fingerprint());
    let immediate_function = module.function(immediate.finish().expect("immediate function"));
    let task_function = module.function(task.finish().expect("task function"));
    let trap_function = module.function(trapping.finish().expect("trap function"));
    module.script_export(ExportDeclaration {
        stable_id: IMMEDIATE_EXPORT,
        function: immediate_function,
        signature: signature.clone(),
        effect: FunctionEffect::Immediate,
    });
    module.script_export(ExportDeclaration {
        stable_id: TASK_EXPORT,
        function: task_function,
        signature: signature.clone(),
        effect: FunctionEffect::Task,
    });
    module.script_export(ExportDeclaration {
        stable_id: TRAP_EXPORT,
        function: trap_function,
        signature,
        effect: FunctionEffect::Immediate,
    });
    verify(module.finish(), VerifierLimits::default()).expect("immediate gate module")
}

#[test]
fn immediate_calls_build_no_task_lifecycle_and_match_the_metered_charge() {
    let verified = immediate_module();
    let schema = verified.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm.load_module(verified, HOST, schema).expect("load");
    let policy = MustCompletePolicy {
        fuel: 256,
        cumulative_budget: 1_024,
    };

    // The metered Task path over an identical function body pins the
    // expected result and fuel charge.
    let scope = realm.create_scope(None).expect("scope");
    let (task_value, task_charge) = realm
        .call_export_metered::<TaskAdd>(module, scope, &41, policy)
        .expect("metered call");
    assert_eq!(task_value, 42);
    let after_task = realm.resource_ledger();

    for round in 0..16_i32 {
        let (value, charge) = realm
            .call_export_immediate::<ImmediateAdd>(module, &round, policy)
            .expect("immediate call");
        assert_eq!(value, round + 1, "immediate result (round {round})");
        assert_eq!(
            charge.fuel_used, task_charge.fuel_used,
            "the immediate path settles identical fuel (round {round})"
        );
        let ledger = realm.resource_ledger();
        assert_eq!(
            (ledger.tasks, ledger.scheduler_tokens, ledger.continuations),
            (
                after_task.tasks,
                after_task.scheduler_tokens,
                after_task.continuations,
            ),
            "no Task, token, or continuation entry appears (round {round})"
        );
    }
    // The continuation storage cycles through the H1 pool instead of
    // accumulating anywhere.
    assert!(realm.continuation_pool_depth() >= 1);
}

#[test]
fn immediate_traps_surface_and_non_immediate_exports_are_rejected() {
    let verified = immediate_module();
    let schema = verified.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm.load_module(verified, HOST, schema).expect("load");
    let policy = MustCompletePolicy {
        fuel: 256,
        cumulative_budget: 1_024,
    };
    let trapped = realm
        .call_export_immediate::<ImmediateTrap>(module, &7, policy)
        .expect_err("division by zero traps");
    assert!(
        matches!(trapped, ScriptCallError::HandlerTrapped(_)),
        "the trap surfaces with its diagnostic payload: {trapped:?}"
    );
    let rejected = realm
        .call_export_immediate::<TaskAdd>(module, &7, policy)
        .expect_err("task exports never take the immediate path");
    assert!(
        matches!(
            rejected,
            ScriptCallError::EffectNotCallable { name: "task_add" }
        ),
        "non-immediate exports are rejected up front: {rejected:?}"
    );
    let ledger = realm.resource_ledger();
    assert_eq!(
        (ledger.tasks, ledger.scheduler_tokens),
        (0, 0),
        "neither path created lifecycle state"
    );
}
