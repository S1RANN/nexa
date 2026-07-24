use std::fmt;

use nexa_verifier::VerifiedModule;

use crate::{
    CheckedContinuation, CheckedInterpreter, InterpreterError, InterpreterOutcome, RuntimeError,
    RuntimeValue, StepConfig, StepResult, TaskHandle, TaskRuntime,
};

#[derive(Clone, Debug)]
pub struct VerifiedTaskContinuation {
    task: TaskHandle,
    continuation: CheckedContinuation,
    charged_fuel: u64,
}

impl VerifiedTaskContinuation {
    #[must_use]
    pub const fn task(&self) -> TaskHandle {
        self.task
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifiedTaskError {
    Runtime(RuntimeError),
    Interpreter(InterpreterError),
}

impl fmt::Display for VerifiedTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for VerifiedTaskError {}

impl From<RuntimeError> for VerifiedTaskError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<InterpreterError> for VerifiedTaskError {
    fn from(error: InterpreterError) -> Self {
        Self::Interpreter(error)
    }
}

impl TaskRuntime {
    pub fn step_verified(
        &mut self,
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        config: StepConfig,
    ) -> Result<
        (
            StepResult<Option<RuntimeValue>>,
            Option<VerifiedTaskContinuation>,
        ),
        VerifiedTaskError,
    > {
        let task = self.admit_task(config.owner, 1, true)?;
        self.poll_task(task)?;
        let outcome = CheckedInterpreter::run(module, function, arguments, config.fuel_slice)
            .map_err(|error| {
                let _ = self.trap_task(task);
                VerifiedTaskError::Interpreter(error)
            })?;
        self.finish_verified_outcome(task, outcome, config, config.fuel_slice)
    }

    pub fn resume_verified(
        &mut self,
        module: &VerifiedModule,
        continuation: VerifiedTaskContinuation,
        config: StepConfig,
    ) -> Result<
        (
            StepResult<Option<RuntimeValue>>,
            Option<VerifiedTaskContinuation>,
        ),
        VerifiedTaskError,
    > {
        self.resume_task(continuation.task)?;
        let outcome =
            CheckedInterpreter::resume(module, continuation.continuation, config.fuel_slice)
                .map_err(|error| {
                    let _ = self.trap_task(continuation.task);
                    VerifiedTaskError::Interpreter(error)
                })?;
        self.finish_verified_outcome(
            continuation.task,
            outcome,
            config,
            continuation.charged_fuel.saturating_add(config.fuel_slice),
        )
    }

    fn finish_verified_outcome(
        &mut self,
        task: TaskHandle,
        outcome: InterpreterOutcome,
        config: StepConfig,
        charged_fuel: u64,
    ) -> Result<
        (
            StepResult<Option<RuntimeValue>>,
            Option<VerifiedTaskContinuation>,
        ),
        VerifiedTaskError,
    > {
        match outcome {
            InterpreterOutcome::Returned(value) => {
                self.finish_task(task)?;
                Ok((StepResult::Completed(value), None))
            }
            InterpreterOutcome::Yielded(continuation) => {
                self.yield_task(task)?;
                if charged_fuel >= config.cumulative_budget {
                    self.resume_task(task)?;
                    self.request_task_cancel(task)?;
                    self.reach_task_safepoint(task)?;
                    self.clean_task(task)?;
                    return Ok((StepResult::Completed(None), None));
                }
                Ok((
                    StepResult::Promoted(task),
                    Some(VerifiedTaskContinuation {
                        task,
                        continuation,
                        charged_fuel,
                    }),
                ))
            }
            InterpreterOutcome::Trapped => {
                self.trap_task(task)?;
                Ok((StepResult::Completed(None), None))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{FunctionBuilder, Instruction, ModuleBuilder, Signature, ValueType};
    use nexa_verifier::{VerifierLimits, verify};

    use crate::{RuntimeLimits, RuntimeValue, StepConfig, StepResult, TaskLimits, TaskRuntime};

    #[test]
    fn verified_task_first_slice_promotes_and_resumes_without_new_admission() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        function
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut runtime = TaskRuntime::new(8, RuntimeLimits::default());
        let scope = runtime.create_scope(None).unwrap();
        let config = StepConfig {
            owner: scope,
            priority: 0,
            fuel_slice: 10,
            cumulative_budget: 100,
            limits: TaskLimits::default(),
        };
        let (result, continuation) = runtime
            .step_verified(&module, 0, &[RuntimeValue::I32(12)], config)
            .unwrap();
        assert!(matches!(result, StepResult::Promoted(_)));
        let (result, continuation) = runtime
            .resume_verified(&module, continuation.unwrap(), config)
            .unwrap();
        assert_eq!(result, StepResult::Completed(Some(RuntimeValue::I32(12))));
        assert!(continuation.is_none());
    }
}
