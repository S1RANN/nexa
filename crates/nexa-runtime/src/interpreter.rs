use std::fmt;

use nexa_bytecode::{Instruction, ValueType};
use nexa_verifier::VerifiedModule;

use crate::{GcRef, RuntimeValue};

#[derive(Clone, Debug)]
struct CheckedFrame {
    function: usize,
    pc: usize,
    registers: Vec<RuntimeValue>,
    return_target: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct CheckedContinuation {
    frames: Vec<CheckedFrame>,
}

impl CheckedContinuation {
    #[must_use]
    pub fn gc_roots(&self) -> Vec<GcRef> {
        self.frames
            .iter()
            .flat_map(|frame| {
                frame.registers.iter().filter_map(|value| match value {
                    RuntimeValue::Ref(reference) => Some(*reference),
                    RuntimeValue::I32(_) | RuntimeValue::Bool(_) | RuntimeValue::Unit => None,
                })
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub enum InterpreterOutcome {
    Returned(Option<RuntimeValue>),
    Yielded(CheckedContinuation),
    Trapped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterpreterError {
    MissingFunction(u32),
    ArgumentCount,
    TypeMismatch,
    RegisterOutOfRange(u16),
    JumpOutOfRange(u32),
    FellOffFunction,
}

impl fmt::Display for InterpreterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for InterpreterError {}

pub struct CheckedInterpreter;

impl CheckedInterpreter {
    pub fn run(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        fuel: u64,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        let frame = make_frame(module, function, arguments, None)?;
        Self::execute(
            module,
            CheckedContinuation {
                frames: vec![frame],
            },
            fuel,
        )
    }

    pub fn resume(
        module: &VerifiedModule,
        continuation: CheckedContinuation,
        fuel: u64,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute(module, continuation, fuel)
    }

    #[allow(clippy::too_many_lines)]
    fn execute(
        module: &VerifiedModule,
        mut continuation: CheckedContinuation,
        mut fuel: u64,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        loop {
            if fuel == 0 {
                return Ok(InterpreterOutcome::Yielded(continuation));
            }
            fuel -= 1;
            let frame = continuation
                .frames
                .last_mut()
                .ok_or(InterpreterError::FellOffFunction)?;
            let function = module.module().functions.get(frame.function).ok_or(
                InterpreterError::MissingFunction(
                    u32::try_from(frame.function).unwrap_or(u32::MAX),
                ),
            )?;
            let instruction = *function
                .code
                .get(frame.pc)
                .ok_or(InterpreterError::FellOffFunction)?;
            match instruction {
                Instruction::LoadI32 { dst, value } => {
                    set_register(frame, dst, RuntimeValue::I32(value))?;
                    frame.pc += 1;
                }
                Instruction::LoadBool { dst, value } => {
                    set_register(frame, dst, RuntimeValue::Bool(value))?;
                    frame.pc += 1;
                }
                Instruction::Move { dst, source } => {
                    let value = register(frame, source)?;
                    set_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Instruction::Add { dst, lhs, rhs }
                | Instruction::Sub { dst, lhs, rhs }
                | Instruction::Mul { dst, lhs, rhs } => {
                    let RuntimeValue::I32(lhs) = register(frame, lhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::I32(rhs) = register(frame, rhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let value = match instruction {
                        Instruction::Add { .. } => lhs.wrapping_add(rhs),
                        Instruction::Sub { .. } => lhs.wrapping_sub(rhs),
                        Instruction::Mul { .. } => lhs.wrapping_mul(rhs),
                        _ => unreachable!(),
                    };
                    set_register(frame, dst, RuntimeValue::I32(value))?;
                    frame.pc += 1;
                }
                Instruction::CompareEq { dst, lhs, rhs } => {
                    let lhs = register(frame, lhs)?;
                    let rhs = register(frame, rhs)?;
                    if runtime_value_type(lhs).is_none()
                        || runtime_value_type(lhs) != runtime_value_type(rhs)
                    {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    set_register(frame, dst, RuntimeValue::Bool(lhs == rhs))?;
                    frame.pc += 1;
                }
                Instruction::Jump { target } => {
                    frame.pc = checked_target(function.code.len(), target)?;
                }
                Instruction::JumpIfFalse { condition, target } => {
                    let RuntimeValue::Bool(condition) = register(frame, condition)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    frame.pc = if condition {
                        frame.pc + 1
                    } else {
                        checked_target(function.code.len(), target)?
                    };
                }
                Instruction::Call {
                    function,
                    args,
                    dst,
                } => {
                    let arguments = (0..args)
                        .map(|register_index| register(frame, register_index))
                        .collect::<Result<Vec<_>, _>>()?;
                    frame.pc += 1;
                    let callee = make_frame(module, function, &arguments, Some(dst))?;
                    continuation.frames.push(callee);
                }
                Instruction::Return { source } => {
                    let result = register(frame, source)?;
                    let completed = continuation
                        .frames
                        .pop()
                        .ok_or(InterpreterError::FellOffFunction)?;
                    if let Some(caller) = continuation.frames.last_mut() {
                        set_register(
                            caller,
                            completed
                                .return_target
                                .ok_or(InterpreterError::TypeMismatch)?,
                            result,
                        )?;
                    } else {
                        return Ok(InterpreterOutcome::Returned(Some(result)));
                    }
                }
                Instruction::ReturnVoid => {
                    continuation
                        .frames
                        .pop()
                        .ok_or(InterpreterError::FellOffFunction)?;
                    if continuation.frames.is_empty() {
                        return Ok(InterpreterOutcome::Returned(None));
                    }
                }
                Instruction::Safepoint => frame.pc += 1,
                Instruction::Yield => {
                    frame.pc += 1;
                    return Ok(InterpreterOutcome::Yielded(continuation));
                }
                Instruction::Trap => return Ok(InterpreterOutcome::Trapped),
            }
        }
    }
}

fn make_frame(
    module: &VerifiedModule,
    function: u32,
    arguments: &[RuntimeValue],
    return_target: Option<u16>,
) -> Result<CheckedFrame, InterpreterError> {
    let function_index =
        usize::try_from(function).map_err(|_| InterpreterError::MissingFunction(function))?;
    let function = module
        .module()
        .functions
        .get(function_index)
        .ok_or(InterpreterError::MissingFunction(function))?;
    if arguments.len() != function.signature.parameters.len() {
        return Err(InterpreterError::ArgumentCount);
    }
    for (argument, expected) in arguments
        .iter()
        .copied()
        .zip(function.signature.parameters.iter().copied())
    {
        if runtime_value_type(argument) != Some(expected) {
            return Err(InterpreterError::TypeMismatch);
        }
    }
    let mut registers = vec![RuntimeValue::Unit; usize::from(function.registers)];
    registers[..arguments.len()].copy_from_slice(arguments);
    Ok(CheckedFrame {
        function: function_index,
        pc: 0,
        registers,
        return_target,
    })
}

fn register(frame: &CheckedFrame, register: u16) -> Result<RuntimeValue, InterpreterError> {
    frame
        .registers
        .get(usize::from(register))
        .copied()
        .ok_or(InterpreterError::RegisterOutOfRange(register))
}

fn set_register(
    frame: &mut CheckedFrame,
    register: u16,
    value: RuntimeValue,
) -> Result<(), InterpreterError> {
    *frame
        .registers
        .get_mut(usize::from(register))
        .ok_or(InterpreterError::RegisterOutOfRange(register))? = value;
    Ok(())
}

const fn runtime_value_type(value: RuntimeValue) -> Option<ValueType> {
    match value {
        RuntimeValue::I32(_) => Some(ValueType::I32),
        RuntimeValue::Bool(_) => Some(ValueType::Bool),
        RuntimeValue::Ref(_) => Some(ValueType::Ref),
        RuntimeValue::Unit => None,
    }
}

fn checked_target(code_len: usize, target: u32) -> Result<usize, InterpreterError> {
    let target_index =
        usize::try_from(target).map_err(|_| InterpreterError::JumpOutOfRange(target))?;
    if target_index < code_len {
        Ok(target_index)
    } else {
        Err(InterpreterError::JumpOutOfRange(target))
    }
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{FunctionBuilder, Instruction, ModuleBuilder, Signature, ValueType};
    use nexa_verifier::{VerifierLimits, verify};

    use super::{CheckedInterpreter, InterpreterOutcome};
    use crate::{GcRoots, Heap, Object, RuntimeValue};

    #[test]
    fn verified_program_yields_and_resumes_without_repeating_add() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32, ValueType::I32],
                result: Some(ValueType::I32),
            },
            3,
        );
        function
            .emit(Instruction::Add {
                dst: 2,
                lhs: 0,
                rhs: 1,
            })
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 2 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let outcome = CheckedInterpreter::run(
            &module,
            0,
            &[RuntimeValue::I32(2), RuntimeValue::I32(5)],
            10,
        )
        .unwrap();
        let InterpreterOutcome::Yielded(continuation) = outcome else {
            panic!("expected yield");
        };
        let outcome = CheckedInterpreter::resume(&module, continuation, 10).unwrap();
        assert!(matches!(
            outcome,
            InterpreterOutcome::Returned(Some(RuntimeValue::I32(7)))
        ));
    }

    #[test]
    fn yielded_reference_is_a_gc_root_and_terminal_return_releases_it() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Ref],
                result: Some(ValueType::Ref),
            },
            1,
        );
        function
            .set_root(0)
            .unwrap()
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut heap = Heap::new(1);
        let reference = heap.allocate(Object::String("root".into())).unwrap();
        let outcome =
            CheckedInterpreter::run(&module, 0, &[RuntimeValue::Ref(reference)], 10).unwrap();
        let InterpreterOutcome::Yielded(continuation) = outcome else {
            panic!("expected yield");
        };
        let roots = GcRoots {
            suspended_tasks: continuation.gc_roots(),
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 1);
        assert!(matches!(
            CheckedInterpreter::resume(&module, continuation, 10).unwrap(),
            InterpreterOutcome::Returned(Some(RuntimeValue::Ref(value))) if value == reference
        ));
        assert_eq!(heap.collect(&GcRoots::default()).unwrap().reclaimed, 1);
    }
}
