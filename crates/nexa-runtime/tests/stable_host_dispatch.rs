use std::sync::{Arc, Mutex};

use nexa_bytecode::{
    AbandonPolicy, AsyncResultType, CancelPolicy, FunctionBuilder, FunctionEffect, HostCallMode,
    HostImport, Instruction, ModuleBuilder, RootMap, ScriptExport, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    HostCallOutcome, HostFunctionAuthority, HostFunctionAuthorityField, HostFunctionSlot,
    HostRegistry, HostTrap, RealmConfig, RealmError, RealmRuntime, ResolvedHostFunction,
    ResourceContext, RuntimeError, RuntimeHost, RuntimeHostArgs, RuntimeValue, StepConfig,
    TaskLimits, TaskPoll,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST_CONTRACT_ID: StableId = StableId(0x5354_4142_4c45_484f);
const DISPATCH_EXPORT_ID: StableId = StableId(0x4449_5350_4154_4348);

struct OrderedContractRegistry {
    functions: [StableId; 3],
    authorities: [HostFunctionAuthority; 3],
    calls: Arc<Mutex<Vec<u32>>>,
}

impl HostRegistry for OrderedContractRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(HOST_CONTRACT_ID)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        if id == self.functions[0] {
            Some(ResolvedHostFunction::new(
                HostFunctionSlot::new(0),
                &self.authorities[0],
            ))
        } else if id == self.functions[1] {
            Some(ResolvedHostFunction::new(
                HostFunctionSlot::new(1),
                &self.authorities[1],
            ))
        } else if id == self.functions[2] {
            Some(ResolvedHostFunction::new(
                HostFunctionSlot::new(2),
                &self.authorities[2],
            ))
        } else {
            None
        }
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        _: &mut ResourceContext<'_>,
        arguments: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if !arguments.is_empty() {
            return Err(HostTrap::Arity);
        }
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(slot.index());
        let value = match slot.index() {
            0 => 1,
            1 => 2,
            2 => 3,
            _ => return Err(HostTrap::InvalidFunctionSlot(slot)),
        };
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::I32(value)))
    }
}

struct SingleAuthorityRegistry {
    authority: HostFunctionAuthority,
}

impl HostRegistry for SingleAuthorityRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(HOST_CONTRACT_ID)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        (id == self.authority.stable_id()).then_some(ResolvedHostFunction::new(
            HostFunctionSlot::new(0),
            &self.authority,
        ))
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

fn verified_import_module(import: HostImport) -> VerifiedModule {
    let parameter_count =
        u16::try_from(import.parameters.len()).expect("test Host signature fits u16");
    let destination = parameter_count;
    let mut entrypoint = FunctionBuilder::new(
        Signature {
            parameters: import.parameters.clone(),
            result: import.result,
        },
        destination + 1,
    );
    entrypoint
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: parameter_count,
            dst: destination,
        })
        .emit(Instruction::Return {
            source: destination,
        });
    let mut entrypoint = entrypoint.finish().expect("forged Host import entrypoint");
    if import.result.is_some_and(ValueType::is_reference) {
        entrypoint.root_bitmap[usize::from(destination)] = true;
        let mut before_call = vec![false; usize::from(destination + 1)];
        for (index, parameter) in import.parameters.iter().enumerate() {
            before_call[index] = parameter.is_reference();
        }
        let mut before_return = before_call.clone();
        before_return[usize::from(destination)] = true;
        entrypoint.safepoints = vec![0, 1];
        entrypoint.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: before_call,
            },
            RootMap {
                pc: 1,
                bitmap: before_return,
            },
        ];
    }

    let schema = nexa_bytecode::StateSchema::default().fingerprint();
    let mut module = ModuleBuilder::new();
    module.metadata(HOST_CONTRACT_ID, schema);
    if let Some(async_result) = import.async_result {
        let result = nexa_bytecode::result_type(async_result.success, async_result.error);
        assert_eq!(result.type_id, async_result.result_type);
        module.enum_type(result);
    }
    module.host_import(import);
    module.function(entrypoint);
    verify(module.finish(), VerifierLimits::default()).expect("verified forged Host import module")
}

fn assert_authority_mismatch(
    authority: HostFunctionAuthority,
    import: HostImport,
    field: HostFunctionAuthorityField,
) {
    let stable_id = authority.stable_id();
    let schema = nexa_bytecode::StateSchema::default().fingerprint();
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        RuntimeHost::new(8),
        Box::new(SingleAuthorityRegistry { authority }),
    )
    .expect("hosted Realm");
    assert_eq!(
        realm.load_module(verified_import_module(import), HOST_CONTRACT_ID, schema),
        Err(RealmError::HostFunctionAuthorityMismatch { stable_id, field })
    );
}

#[test]
fn module_local_host_import_resolves_to_registry_dense_slot() {
    let contract_functions = [
        StableId::from_name("DispatchContract::first"),
        StableId::from_name("DispatchContract::second"),
        StableId::from_name("DispatchContract::third"),
    ];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let authorities = contract_functions.map(|stable_id| {
        HostFunctionAuthority::new(
            stable_id,
            [0; 32],
            &[],
            Some(ValueType::I32),
            HostCallMode::Immediate,
            1,
            None,
            &[],
        )
    });

    let signature = Signature {
        parameters: Vec::new(),
        result: Some(ValueType::I32),
    };
    let mut entrypoint = FunctionBuilder::new(signature.clone(), 1);
    entrypoint
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 0,
            dst: 0,
        })
        .emit(Instruction::Return { source: 0 });

    let schema = nexa_bytecode::StateSchema::default().fingerprint();
    let mut module = ModuleBuilder::new();
    module.metadata(HOST_CONTRACT_ID, schema);
    module.host_import(HostImport {
        stable_id: contract_functions[2],
        declaration_fingerprint: [0; 32],
        capabilities: Vec::new(),
        parameters: Vec::new(),
        result: Some(ValueType::I32),
        mode: HostCallMode::Immediate,
        fuel_cost: 1,
        async_result: None,
    });
    let function = module.function(entrypoint.finish().expect("host dispatch entrypoint"));
    module.script_export(ScriptExport {
        stable_id: DISPATCH_EXPORT_ID,
        function,
        effect: FunctionEffect::Task,
        signature,
    });
    let verified = verify(module.finish(), VerifierLimits::default()).expect("verified module");
    assert_eq!(verified.module().host_imports.len(), 1);
    assert_eq!(
        verified.module().host_imports[0].stable_id,
        contract_functions[2]
    );

    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        RuntimeHost::new(8),
        Box::new(OrderedContractRegistry {
            functions: contract_functions,
            authorities,
            calls: Arc::clone(&calls),
        }),
    )
    .expect("hosted realm");
    let module = realm
        .load_module(verified, HOST_CONTRACT_ID, schema)
        .expect("loaded module");
    let scope = realm.create_scope(None).expect("scope");
    let task = realm
        .spawn_task(
            module,
            DISPATCH_EXPORT_ID,
            &[],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 32,
                cumulative_budget: 128,
                limits: TaskLimits::default(),
            },
        )
        .expect("spawned task");

    assert_eq!(
        realm.poll_task(task, 32).expect("completed host call"),
        TaskPoll::Completed(RuntimeValue::I32(3))
    );
    assert_eq!(
        *calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![2]
    );
}

#[test]
fn realm_rejects_spawning_internal_unexported_function() {
    const INTERNAL_FUNCTION_ID: StableId = StableId(0x494e_5445_524e_414c);

    let signature = Signature {
        parameters: Vec::new(),
        result: Some(ValueType::I32),
    };
    let mut internal = FunctionBuilder::new(signature, 1);
    internal
        .effect(FunctionEffect::Task)
        .emit(Instruction::LoadI32 { dst: 0, value: 7 })
        .emit(Instruction::Return { source: 0 });
    let schema = nexa_bytecode::StateSchema::default().fingerprint();
    let mut module = ModuleBuilder::new();
    module.metadata(HOST_CONTRACT_ID, schema);
    module.function(internal.finish().expect("internal task function"));
    let verified = verify(module.finish(), VerifierLimits::default()).expect("verified module");

    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm
        .load_module(verified, HOST_CONTRACT_ID, schema)
        .expect("loaded module");
    let scope = realm.create_scope(None).expect("scope");
    assert_eq!(
        realm.spawn_task(
            module,
            INTERNAL_FUNCTION_ID,
            &[],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 32,
                cumulative_budget: 128,
                limits: TaskLimits::default(),
            },
        ),
        Err(RuntimeError::Realm(Box::new(
            RealmError::MissingScriptExport(INTERNAL_FUNCTION_ID)
        )))
    );
}

#[test]
fn realm_rejects_host_import_with_forged_declaration_fingerprint() {
    let stable_id = StableId::from_name("AuthorityContract::fingerprint");
    let authority = HostFunctionAuthority::new(
        stable_id,
        [5; 32],
        &[],
        Some(ValueType::I32),
        HostCallMode::Immediate,
        1,
        None,
        &[],
    );
    assert_authority_mismatch(
        authority,
        HostImport {
            stable_id,
            declaration_fingerprint: [6; 32],
            capabilities: Vec::new(),
            parameters: Vec::new(),
            result: Some(ValueType::I32),
            mode: HostCallMode::Immediate,
            fuel_cost: 1,
            async_result: None,
        },
        HostFunctionAuthorityField::DeclarationFingerprint,
    );
}

#[test]
fn realm_rejects_host_import_with_forged_capabilities() {
    let stable_id = StableId::from_name("AuthorityContract::capabilities");
    let authority = HostFunctionAuthority::new(
        stable_id,
        [7; 32],
        &[],
        Some(ValueType::I32),
        HostCallMode::Immediate,
        1,
        None,
        &["clock"],
    );
    assert_authority_mismatch(
        authority,
        HostImport {
            stable_id,
            declaration_fingerprint: [7; 32],
            capabilities: Vec::new(),
            parameters: Vec::new(),
            result: Some(ValueType::I32),
            mode: HostCallMode::Immediate,
            fuel_cost: 1,
            async_result: None,
        },
        HostFunctionAuthorityField::Capabilities,
    );
}

#[test]
fn realm_rejects_host_import_with_forged_lower_fuel() {
    let stable_id = StableId::from_name("AuthorityContract::metered");
    let authority = HostFunctionAuthority::new(
        stable_id,
        [1; 32],
        &[],
        Some(ValueType::I32),
        HostCallMode::Immediate,
        8,
        None,
        &["clock"],
    );
    assert_authority_mismatch(
        authority,
        HostImport {
            stable_id,
            declaration_fingerprint: [1; 32],
            capabilities: vec!["clock".to_owned()],
            parameters: Vec::new(),
            result: Some(ValueType::I32),
            mode: HostCallMode::Immediate,
            fuel_cost: 1,
            async_result: None,
        },
        HostFunctionAuthorityField::FuelCost,
    );
}

#[test]
fn realm_rejects_host_import_with_forged_mode() {
    let stable_id = StableId::from_name("AuthorityContract::asynchronous");
    let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
    let async_result = AsyncResultType {
        result_type: result.type_id,
        success: ValueType::I32,
        error: ValueType::I32,
        cancel_policy: CancelPolicy::ReturnError,
        abandon_policy: AbandonPolicy::Trap,
        cancel_error: Some(1),
        abandon_error: None,
    };
    let authority = HostFunctionAuthority::new(
        stable_id,
        [2; 32],
        &[],
        Some(ValueType::Named(result.type_id)),
        HostCallMode::Async,
        3,
        Some(async_result),
        &[],
    );
    assert_authority_mismatch(
        authority,
        HostImport {
            stable_id,
            declaration_fingerprint: [2; 32],
            capabilities: Vec::new(),
            parameters: Vec::new(),
            result: Some(ValueType::Named(result.type_id)),
            mode: HostCallMode::Immediate,
            fuel_cost: 3,
            async_result: None,
        },
        HostFunctionAuthorityField::Mode,
    );
}

#[test]
fn realm_rejects_host_import_with_forged_signature() {
    let stable_id = StableId::from_name("AuthorityContract::signature");
    let authority = HostFunctionAuthority::new(
        stable_id,
        [3; 32],
        &[],
        Some(ValueType::I32),
        HostCallMode::Immediate,
        5,
        None,
        &[],
    );
    assert_authority_mismatch(
        authority,
        HostImport {
            stable_id,
            declaration_fingerprint: [3; 32],
            capabilities: Vec::new(),
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
            mode: HostCallMode::Immediate,
            fuel_cost: 5,
            async_result: None,
        },
        HostFunctionAuthorityField::Parameters,
    );
}

#[test]
fn realm_rejects_host_import_missing_from_registry_authority() {
    let registered = StableId::from_name("AuthorityContract::registered");
    let missing = StableId::from_name("AuthorityContract::missing");
    let authority = HostFunctionAuthority::new(
        registered,
        [4; 32],
        &[],
        Some(ValueType::I32),
        HostCallMode::Immediate,
        1,
        None,
        &[],
    );
    let schema = nexa_bytecode::StateSchema::default().fingerprint();
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        RuntimeHost::new(8),
        Box::new(SingleAuthorityRegistry { authority }),
    )
    .expect("hosted Realm");
    assert_eq!(
        realm.load_module(
            verified_import_module(HostImport {
                stable_id: missing,
                declaration_fingerprint: [0; 32],
                capabilities: Vec::new(),
                parameters: Vec::new(),
                result: Some(ValueType::I32),
                mode: HostCallMode::Immediate,
                fuel_cost: 1,
                async_result: None,
            }),
            HOST_CONTRACT_ID,
            schema,
        ),
        Err(RealmError::MissingHostFunctionAuthority(missing))
    );
}
