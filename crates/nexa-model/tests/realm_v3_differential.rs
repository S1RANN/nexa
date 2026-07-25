use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_model::realm_v3::{
    RealmV3Config, RealmV3Event, RealmV3ModuleState, RealmV3RequestState, RealmV3TaskState,
    RealmV3World, explore_realm_v3,
};
use nexa_runtime::{
    ActivationEntry, HostErrorPayload, HostPayload, HostRequestHandle, HostRequestState,
    ModuleHandle, ModuleLifecycle, PendingHostRequest, RealmConfig, RealmRuntime,
    RetiredEpochState, RuntimeHost, RuntimeValue, StepConfig, TaskHandle, TaskLimits,
    TaskTerminalReason, TickBudget,
};
use nexa_verifier::{VerifierLimits, verify};

#[test]
fn every_realm_v3_shortest_path_replays_against_runtime() {
    let report = explore_realm_v3(RealmV3Config {
        max_depth: 14,
        max_worlds: 4_096,
    });
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(
        !report.truncated,
        "truncated after {} worlds",
        report.visited_worlds
    );
    assert!(report.visited_worlds >= 20);
    for path in report.shortest_paths {
        let replay_path = path.clone();
        let mut model = RealmV3World::default();
        let mut runtime = RuntimeAdapter::new();
        runtime.assert_matches(model).unwrap();
        for event in path {
            model.apply(event).unwrap();
            runtime
                .apply(event)
                .unwrap_or_else(|error| panic!("{error}; event={event:?}; path={replay_path:?}"));
            runtime
                .assert_matches(model)
                .unwrap_or_else(|error| panic!("{error}; event={event:?}; path={replay_path:?}"));
        }
    }
}

struct RuntimeAdapter {
    realm: RealmRuntime,
    host: RuntimeHost,
    old: ModuleHandle,
    candidate: Option<ModuleHandle>,
    scopes: [Option<nexa_runtime::ScopeHandle>; 2],
    tasks: [Option<TaskHandle>; 2],
    pending: [Option<PendingHostRequest>; 2],
    requests: [Option<HostRequestHandle>; 2],
}

impl RuntimeAdapter {
    fn new() -> Self {
        let host_hash = host_hash();
        let schema_hash = schema_hash();
        let host = RuntimeHost::new(16);
        let mut realm =
            RealmRuntime::with_runtime_host(RealmConfig::default(), host.clone(), Box::new(NoHost));
        let old = realm
            .load_module(old_module(), host_hash, schema_hash)
            .unwrap();
        Self {
            realm,
            host,
            old,
            candidate: None,
            scopes: [None; 2],
            tasks: [None; 2],
            pending: std::array::from_fn(|_| None),
            requests: [None; 2],
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply(&mut self, event: RealmV3Event) -> Result<(), String> {
        match event {
            RealmV3Event::CreateScope => {
                let scope = self
                    .scopes
                    .iter()
                    .position(Option::is_none)
                    .expect("model has a free scope slot");
                self.scopes[scope] = Some(self.realm.create_scope(None).map_err(debug)?);
            }
            RealmV3Event::SpawnTask => {
                let task = self
                    .tasks
                    .iter()
                    .position(Option::is_none)
                    .expect("model has a free task slot");
                let owner = self.scopes[task]
                    .or(self.scopes[0])
                    .expect("model creates a scope first");
                let module = self
                    .candidate
                    .filter(|candidate| {
                        self.realm.module_lifecycle(*candidate).ok()
                            == Some(ModuleLifecycle::Active)
                    })
                    .unwrap_or(self.old);
                self.tasks[task] = Some(
                    self.realm
                        .call(
                            module,
                            0,
                            &[RuntimeValue::I32(7)],
                            StepConfig {
                                owner,
                                priority: 1,
                                fuel_slice: 64,
                                cumulative_budget: 1_024,
                                limits: TaskLimits::default(),
                            },
                        )
                        .map_err(debug)?,
                );
                if self.realm.resource_snapshot().tokens == 0 {
                    self.realm
                        .create_resource_token(
                            self.tasks[task].expect("task was stored"),
                            nexa_runtime::RuntimeHostDomain::Render,
                        )
                        .map_err(debug)?;
                    self.realm
                        .create_snapshot(
                            self.tasks[task].expect("task was stored"),
                            Arc::<[i32]>::from([1, 2, 3]),
                        )
                        .map_err(debug)?;
                }
            }
            RealmV3Event::BeginHostRequest => {
                let index = self
                    .tasks
                    .iter()
                    .enumerate()
                    .position(|(index, task)| {
                        task.is_some_and(|task| {
                            self.realm.task_snapshot(task).is_ok_and(|snapshot| {
                                snapshot.state == nexa_runtime::TaskState::Ready
                            })
                        }) && self.requests[index].is_none()
                    })
                    .expect("model has a ready task");
                let task = self.tasks[index].expect("task slot is live");
                let pending = self.realm.create_host_request(task).map_err(debug)?;
                self.realm
                    .wait_for_request(task, pending.request)
                    .map_err(debug)?;
                self.requests[index] = Some(pending.request);
                self.pending[index] = Some(pending);
            }
            RealmV3Event::HostSuccess => {
                let index = self.waiting_request_index();
                self.pending[index]
                    .as_mut()
                    .expect("model owns ticket")
                    .ticket
                    .complete(HostPayload::I32(9))
                    .map_err(debug)?;
                self.tick()?;
                self.pending[index] = None;
            }
            RealmV3Event::HostError => {
                let index = self.waiting_request_index();
                self.pending[index]
                    .as_mut()
                    .expect("model owns ticket")
                    .ticket
                    .fail(HostErrorPayload {
                        code: 7,
                        message: Some("model host failure".into()),
                    })
                    .map_err(debug)?;
                self.tick()?;
                self.pending[index] = None;
            }
            RealmV3Event::HostCancel => {
                let index = self.waiting_request_index();
                self.pending[index]
                    .as_mut()
                    .expect("model owns ticket")
                    .ticket
                    .cancelled()
                    .map_err(debug)?;
                self.tick()?;
                self.pending[index] = None;
            }
            RealmV3Event::HostAbandon => {
                let index = self.waiting_request_index();
                self.pending[index]
                    .as_mut()
                    .expect("model owns ticket")
                    .ticket
                    .abandon()
                    .map_err(debug)?;
                self.tick()?;
                self.pending[index] = None;
            }
            RealmV3Event::BeginReload => {
                let candidate = self
                    .realm
                    .prepare_reload(self.old, candidate_module(), host_hash(), schema_hash())
                    .map_err(debug)?;
                self.realm.quiesce_reload().map_err(debug)?;
                self.candidate = Some(candidate);
            }
            RealmV3Event::RollbackPreCommit => {
                self.realm.rollback_reload().map_err(debug)?;
                self.candidate = None;
            }
            RealmV3Event::PublishActivationSuccess => {
                self.realm
                    .stage_reload(0, &[RuntimeValue::I32(7)])
                    .map_err(debug)?;
                self.realm
                    .commit_reload(ActivationEntry {
                        function_id: 1,
                        arguments: &[],
                        fuel: 64,
                    })
                    .map_err(debug)?;
                self.tick()?;
            }
            RealmV3Event::PublishActivationFailure => {
                self.realm
                    .stage_reload(0, &[RuntimeValue::I32(7)])
                    .map_err(debug)?;
                assert!(
                    self.realm
                        .commit_reload(ActivationEntry {
                            function_id: 2,
                            arguments: &[],
                            fuel: 64,
                        })
                        .is_err()
                );
                self.tick()?;
            }
            RealmV3Event::LateHostSuccess => {
                let index =
                    self.requests
                        .iter()
                        .enumerate()
                        .position(|(index, request)| {
                            self.pending[index].is_some()
                                && request.is_some_and(|request| {
                                    self.realm.request_terminal_record(request).is_some_and(
                                        |record| record.state == HostRequestState::Detached,
                                    )
                                })
                        })
                        .expect("model has a detached request");
                self.pending[index]
                    .as_mut()
                    .expect("detached request retains ticket")
                    .ticket
                    .complete(HostPayload::I32(9))
                    .map_err(debug)?;
                self.tick()?;
                self.pending[index] = None;
            }
            RealmV3Event::DrainHostReleases => {
                let _ = self.host.drain_releases();
            }
        }
        Ok(())
    }

    fn waiting_request_index(&self) -> usize {
        self.tasks
            .iter()
            .enumerate()
            .position(|(index, task)| {
                task.is_some_and(|task| {
                    self.realm
                        .task_snapshot(task)
                        .is_ok_and(|snapshot| snapshot.state == nexa_runtime::TaskState::Waiting)
                }) && self.pending[index].is_some()
            })
            .expect("model has a waiting request")
    }

    fn tick(&mut self) -> Result<(), String> {
        self.realm
            .tick(TickBudget {
                max_tasks: 0,
                frame_fuel_budget: 0,
                collect_garbage: false,
            })
            .map_err(debug)?;
        Ok(())
    }

    fn assert_matches(&self, model: RealmV3World) -> Result<(), String> {
        for index in 0..model.scopes.len() {
            if model.scopes[index] != self.scopes[index].is_some() {
                return Err(format!("scope {index} presence differs"));
            }
        }
        for index in 0..model.tasks.len() {
            self.assert_task(index, model.tasks[index])?;
            self.assert_request(index, model.requests[index])?;
        }
        self.assert_module(self.old, model.modules[0])?;
        if let Some(candidate) = self.candidate {
            self.assert_module(candidate, model.modules[1])?;
        } else if model.modules[1] != RealmV3ModuleState::Absent {
            return Err("model candidate has no runtime handle".into());
        }
        let resources = self.realm.resource_snapshot();
        let expected_owned_resources = usize::from(model.resource_owner.is_some());
        if resources.tokens != expected_owned_resources
            || resources.snapshots != expected_owned_resources
        {
            return Err(format!(
                "owned resource counts differ: runtime tokens={} snapshots={} model={expected_owned_resources}",
                resources.tokens, resources.snapshots
            ));
        }
        if resources.completion_reservations != usize::from(model.completion_reservations) {
            return Err(format!(
                "completion reservations differ: runtime={} model={}",
                resources.completion_reservations, model.completion_reservations
            ));
        }
        if resources.release_records != usize::from(model.realm_release_records) {
            return Err("realm release counts differ".into());
        }
        if self.host.pending_releases() != usize::from(model.host_release_records) {
            return Err(format!(
                "host release counts differ: runtime={} model={}",
                self.host.pending_releases(),
                model.host_release_records
            ));
        }
        if self.realm.discarded_late_host_results() != u64::from(model.discarded_late_results) {
            return Err("late completion counts differ".into());
        }
        Ok(())
    }

    fn assert_task(&self, index: usize, expected: RealmV3TaskState) -> Result<(), String> {
        let Some(task) = self.tasks[index] else {
            return (expected == RealmV3TaskState::Absent)
                .then_some(())
                .ok_or_else(|| "model task has no runtime task".into());
        };
        let live = self
            .realm
            .task_snapshot(task)
            .ok()
            .map(|snapshot| snapshot.state);
        let terminal = self
            .realm
            .terminal_record(task)
            .map(|record| &record.reason);
        let matches = match expected {
            RealmV3TaskState::Ready => live == Some(nexa_runtime::TaskState::Ready),
            RealmV3TaskState::Running => live == Some(nexa_runtime::TaskState::Running),
            RealmV3TaskState::Waiting => live == Some(nexa_runtime::TaskState::Waiting),
            RealmV3TaskState::ReloadPaused => live == Some(nexa_runtime::TaskState::ReloadPaused),
            RealmV3TaskState::Cancelled => {
                matches!(terminal, Some(TaskTerminalReason::Cancelled(_)))
            }
            RealmV3TaskState::Trapped => {
                matches!(terminal, Some(TaskTerminalReason::Trapped(_)))
            }
            RealmV3TaskState::Completed => {
                matches!(terminal, Some(TaskTerminalReason::Completed(_)))
            }
            RealmV3TaskState::Absent => false,
        };
        matches.then_some(()).ok_or_else(|| {
            format!("task differs: expected={expected:?} live={live:?} terminal={terminal:?}")
        })
    }

    fn assert_request(&self, index: usize, expected: RealmV3RequestState) -> Result<(), String> {
        let Some(request) = self.requests[index] else {
            return (expected == RealmV3RequestState::Absent)
                .then_some(())
                .ok_or_else(|| "model request has no runtime request".into());
        };
        let terminal = self
            .realm
            .request_terminal_record(request)
            .map(|record| record.state);
        let resources = self.realm.resource_snapshot();
        let matches = match expected {
            RealmV3RequestState::InFlight => resources.requests > 0 && terminal.is_none(),
            RealmV3RequestState::Completed => terminal == Some(HostRequestState::Completed),
            RealmV3RequestState::Failed => terminal == Some(HostRequestState::Failed),
            RealmV3RequestState::Cancelled => terminal == Some(HostRequestState::Cancelled),
            RealmV3RequestState::Abandoned => terminal == Some(HostRequestState::Abandoned),
            RealmV3RequestState::Detached => terminal == Some(HostRequestState::Detached),
            RealmV3RequestState::Absent => false,
        };
        matches
            .then_some(())
            .ok_or_else(|| format!("request differs: expected={expected:?} terminal={terminal:?}"))
    }

    fn assert_module(
        &self,
        module: ModuleHandle,
        expected: RealmV3ModuleState,
    ) -> Result<(), String> {
        let lifecycle = self.realm.module_lifecycle(module).ok();
        let retired = self
            .realm
            .retired_epochs()
            .iter()
            .find(|entry| entry.module == module);
        let matches = match expected {
            RealmV3ModuleState::Absent => lifecycle.is_none(),
            RealmV3ModuleState::Staging => lifecycle == Some(ModuleLifecycle::Staging),
            RealmV3ModuleState::Active => lifecycle == Some(ModuleLifecycle::Active),
            RealmV3ModuleState::ActivationFaulted => {
                lifecycle == Some(ModuleLifecycle::ActivationFaulted)
            }
            RealmV3ModuleState::Retired => {
                lifecycle == Some(ModuleLifecycle::Retired)
                    && retired.is_some_and(|entry| entry.state == RetiredEpochState::Retired)
            }
            RealmV3ModuleState::Drained => {
                lifecycle.is_none()
                    && retired.is_some_and(|entry| entry.state == RetiredEpochState::Drained)
            }
        };
        matches
            .then_some(())
            .ok_or_else(|| format!("module differs: expected={expected:?} lifecycle={lifecycle:?}"))
    }
}

struct NoHost;

impl nexa_runtime::HostRegistry for NoHost {
    fn call(
        &mut self,
        id: u32,
        _: &mut nexa_runtime::ResourceContext<'_>,
        _: nexa_runtime::HostArgs<'_>,
    ) -> Result<nexa_runtime::HostCallOutcome, nexa_runtime::HostTrap> {
        Err(nexa_runtime::HostTrap::UnknownFunction(id))
    }
}

fn old_module() -> nexa_verifier::VerifiedModule {
    let mut task = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    task.effect(FunctionEffect::Task)
        .emit(Instruction::Return { source: 0 });
    verified(vec![task.finish().unwrap()])
}

fn candidate_module() -> nexa_verifier::VerifiedModule {
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
        .emit(Instruction::ReturnVoid);
    let mut activation_fault = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    activation_fault
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::Trap);
    verified(vec![
        migration.finish().unwrap(),
        activation.finish().unwrap(),
        activation_fault.finish().unwrap(),
    ])
}

fn verified(functions: Vec<nexa_bytecode::Function>) -> nexa_verifier::VerifiedModule {
    let mut module = ModuleBuilder::new();
    module.metadata(host_hash(), schema_hash());
    for function in functions {
        module.function(function);
    }
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

fn host_hash() -> StableId {
    StableId::from_name("realm-v3-host")
}

fn schema_hash() -> StableId {
    StableId::from_name("realm-v3-schema")
}

fn debug(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}
use std::sync::Arc;
