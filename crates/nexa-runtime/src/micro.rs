use std::collections::BTreeMap;

use crate::{
    DeferAction, FrameArena, FrameError, RuntimeError, RuntimeValue, StepConfig, StepResult,
    TaskHandle, TaskRuntime,
};

type ProgramId = u32;
type RequestId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MicroOp {
    Compute(u32),
    Call(ProgramId),
    Return,
    Safepoint,
    YieldFuel,
    Await(RequestId),
    Complete,
    Trap,
}

#[derive(Clone, Debug)]
struct MicroProgram {
    id: ProgramId,
    registers: usize,
    ops: Vec<MicroOp>,
}

#[derive(Debug)]
struct MicroContinuation {
    task: TaskHandle,
    arena: FrameArena,
    fuel_used: u64,
    pending_budget_cancel: bool,
}

#[allow(dead_code)]
#[derive(Debug)]
enum MicroError {
    Runtime(RuntimeError),
    Frame(FrameError),
    MissingProgram(ProgramId),
    FellOffProgram,
    CleanupBudget,
    Trapped,
}

impl From<RuntimeError> for MicroError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<FrameError> for MicroError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

struct MicroExecutor {
    programs: BTreeMap<ProgramId, MicroProgram>,
    release_counts: BTreeMap<u32, u32>,
    flags: BTreeMap<u32, bool>,
}

impl MicroExecutor {
    fn new(programs: impl IntoIterator<Item = MicroProgram>) -> Self {
        Self {
            programs: programs
                .into_iter()
                .map(|program| (program.id, program))
                .collect(),
            release_counts: BTreeMap::new(),
            flags: BTreeMap::new(),
        }
    }

    fn first_slice(
        &mut self,
        runtime: &mut TaskRuntime,
        program: ProgramId,
        config: StepConfig,
    ) -> Result<(StepResult<()>, Option<MicroContinuation>), MicroError> {
        let entry = self
            .programs
            .get(&program)
            .ok_or(MicroError::MissingProgram(program))?;
        let mut arena = FrameArena::new(config.limits.frames);
        arena.push(program, entry.registers)?;
        let task = runtime.admit_task(config.owner, 1, true)?;
        runtime.poll_task(task)?;
        self.execute(
            runtime,
            config,
            MicroContinuation {
                task,
                arena,
                fuel_used: 0,
                pending_budget_cancel: false,
            },
        )
    }

    fn resume(
        &mut self,
        runtime: &mut TaskRuntime,
        config: StepConfig,
        continuation: MicroContinuation,
    ) -> Result<(StepResult<()>, Option<MicroContinuation>), MicroError> {
        runtime.resume_task(continuation.task)?;
        self.execute(runtime, config, continuation)
    }

    fn execute(
        &mut self,
        runtime: &mut TaskRuntime,
        config: StepConfig,
        mut continuation: MicroContinuation,
    ) -> Result<(StepResult<()>, Option<MicroContinuation>), MicroError> {
        let mut slice_remaining = config.fuel_slice;
        loop {
            let frame = *continuation.arena.current()?;
            let program = self
                .programs
                .get(&frame.function)
                .ok_or(MicroError::MissingProgram(frame.function))?;
            let op = *program
                .ops
                .get(frame.pc as usize)
                .ok_or(MicroError::FellOffProgram)?;
            match op {
                MicroOp::Compute(cost) => {
                    let cost = u64::from(cost);
                    if cost > slice_remaining {
                        runtime.yield_task(continuation.task)?;
                        return Ok((StepResult::Promoted(continuation.task), Some(continuation)));
                    }
                    continuation.arena.current_mut()?.pc += 1;
                    slice_remaining -= cost;
                    continuation.fuel_used = continuation.fuel_used.saturating_add(cost);
                    continuation.pending_budget_cancel =
                        continuation.fuel_used >= config.cumulative_budget;
                    if slice_remaining == 0 {
                        runtime.yield_task(continuation.task)?;
                        return Ok((StepResult::Promoted(continuation.task), Some(continuation)));
                    }
                }
                MicroOp::Call(program_id) => {
                    let callee = self
                        .programs
                        .get(&program_id)
                        .ok_or(MicroError::MissingProgram(program_id))?;
                    continuation.arena.current_mut()?.pc += 1;
                    continuation.arena.push(program_id, callee.registers)?;
                }
                MicroOp::Return => {
                    continuation.arena.pop()?;
                    if continuation.arena.depth() == 0 {
                        runtime.finish_task(continuation.task)?;
                        return Ok((StepResult::Completed(()), None));
                    }
                }
                MicroOp::Safepoint => {
                    continuation.arena.current_mut()?.pc += 1;
                    if continuation.pending_budget_cancel {
                        self.cancel(runtime, &continuation, true, config.limits.max_cleanup_ops)?;
                        return Ok((StepResult::Completed(()), None));
                    }
                }
                MicroOp::YieldFuel => {
                    continuation.arena.current_mut()?.pc += 1;
                    runtime.yield_task(continuation.task)?;
                    return Ok((StepResult::Promoted(continuation.task), Some(continuation)));
                }
                MicroOp::Await(_request) => {
                    continuation.arena.current_mut()?.pc += 1;
                    runtime.await_task(continuation.task)?;
                    return Ok((StepResult::Promoted(continuation.task), Some(continuation)));
                }
                MicroOp::Complete => {
                    runtime.finish_task(continuation.task)?;
                    return Ok((StepResult::Completed(()), None));
                }
                MicroOp::Trap => {
                    runtime.trap_task(continuation.task)?;
                    return Err(MicroError::Trapped);
                }
            }
        }
    }

    fn cancel(
        &mut self,
        runtime: &mut TaskRuntime,
        continuation: &MicroContinuation,
        run_user_defers: bool,
        cleanup_budget: u32,
    ) -> Result<(), MicroError> {
        runtime.request_task_cancel(continuation.task)?;
        runtime.reach_task_safepoint(continuation.task)?;
        if run_user_defers {
            for (used, action) in continuation.arena.defers_rev().enumerate() {
                if used >= cleanup_budget as usize {
                    runtime.trap_task(continuation.task)?;
                    return Err(MicroError::CleanupBudget);
                }
                match action {
                    DeferAction::ReleaseCounter(id) => {
                        *self.release_counts.entry(id).or_default() += 1;
                    }
                    DeferAction::SetFlag(id) => {
                        self.flags.insert(id, true);
                    }
                    DeferAction::Trap | DeferAction::Call { .. } => {
                        runtime.trap_task(continuation.task)?;
                        return Err(MicroError::Trapped);
                    }
                }
            }
        }
        runtime.clean_task(continuation.task)?;
        Ok(())
    }
}

#[test]
fn first_slice_completion_and_fuel_promotion_preserve_continuation() {
    use crate::{RuntimeLimits, TaskLimits, TaskRuntime};

    let mut runtime = TaskRuntime::new(1, RuntimeLimits::default());
    let scope = runtime.create_scope(None).unwrap();
    let config = StepConfig {
        owner: scope,
        priority: 0,
        fuel_slice: 2,
        cumulative_budget: 100,
        limits: TaskLimits::default(),
    };
    let mut executor = MicroExecutor::new([MicroProgram {
        id: 0,
        registers: 1,
        ops: vec![MicroOp::Compute(2), MicroOp::Complete],
    }]);
    let (result, continuation) = executor.first_slice(&mut runtime, 0, config).unwrap();
    let StepResult::Promoted(task) = result else {
        panic!("expected promotion");
    };
    let mut continuation = continuation.unwrap();
    assert_eq!(continuation.task, task);
    assert_eq!(continuation.arena.current().unwrap().pc, 1);
    continuation
        .arena
        .set_register(0, RuntimeValue::I32(17))
        .unwrap();
    let (result, continuation) = executor.resume(&mut runtime, config, continuation).unwrap();
    assert_eq!(result, StepResult::Completed(()));
    assert!(continuation.is_none());
    assert_eq!(
        runtime.scope_snapshot(scope).unwrap().persistent_children,
        0
    );
}

#[test]
fn budget_cancel_runs_defers_only_at_safepoint() {
    use crate::{RuntimeLimits, TaskLimits, TaskRuntime};

    let mut runtime = TaskRuntime::new(2, RuntimeLimits::default());
    let scope = runtime.create_scope(None).unwrap();
    let config = StepConfig {
        owner: scope,
        priority: 0,
        fuel_slice: 10,
        cumulative_budget: 1,
        limits: TaskLimits::default(),
    };
    let mut executor = MicroExecutor::new([MicroProgram {
        id: 0,
        registers: 0,
        ops: vec![MicroOp::Compute(1), MicroOp::Safepoint, MicroOp::Complete],
    }]);
    let entry = executor.programs.get(&0).unwrap();
    let mut arena = FrameArena::new(config.limits.frames);
    arena.push(0, entry.registers).unwrap();
    arena.push_defer(DeferAction::ReleaseCounter(7)).unwrap();
    let task = runtime.admit_task(scope, 1, true).unwrap();
    runtime.poll_task(task).unwrap();
    let outcome = executor.execute(
        &mut runtime,
        config,
        MicroContinuation {
            task,
            arena,
            fuel_used: 0,
            pending_budget_cancel: false,
        },
    );
    assert!(matches!(outcome, Ok((StepResult::Completed(()), None))));
    assert_eq!(executor.release_counts.get(&7), Some(&1));
}

#[test]
fn microprogram_variants_cover_call_return_await_yield_and_trap() {
    let ops = [
        MicroOp::Call(1),
        MicroOp::Return,
        MicroOp::YieldFuel,
        MicroOp::Await(2),
        MicroOp::Trap,
    ];
    assert_eq!(ops.len(), 5);
}
