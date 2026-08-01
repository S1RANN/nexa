use nexa_bytecode::{FunctionEffect, Instruction};
use nexa_compiler::{AnalysisDiagnosticSource, CompileError};
use nexa_core::FileId;
use nexa_diagnostics::ErrorCode;
use nexa_runtime::{CheckedInterpreter, InterpreterOutcome, OpcodeCostTable, RuntimeValue};

#[test]
fn postfix_await_and_yield_lower_to_the_runtime_task_model() {
    let verified = nexa_compiler::compile(
        r"
async fn produce() -> i32 {
    yield;
    return 41;
}

async fn consume() -> i32 {
    let value: i32 = produce().await;
    return value + 1;
}
",
    )
    .expect("postfix await must compile through the canonical typed pipeline");
    let module = verified.module();
    assert_eq!(module.functions.len(), 2);
    assert!(
        module
            .functions
            .iter()
            .all(|function| function.effect == FunctionEffect::Task)
    );
    assert!(
        module.functions[1]
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::Call { function: 0, .. }))
    );
    assert!(
        module.functions[0]
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::Yield))
    );

    let suspended = CheckedInterpreter::run(&verified, 1, &[], 10_000)
        .expect("the nested task reaches its explicit yield");
    let (continuation, fuel) = match suspended {
        InterpreterOutcome::Suspended {
            continuation, fuel, ..
        } => (continuation, fuel),
        other => panic!("async consumer must suspend at the nested yield, got {other:?}"),
    };
    let resumed =
        CheckedInterpreter::poll(&verified, continuation, fuel, &OpcodeCostTable::default())
            .expect("the awaited task resumes");
    assert!(matches!(
        resumed,
        InterpreterOutcome::Returned {
            value: Some(RuntimeValue::I32(42)),
            ..
        }
    ));
}

#[test]
fn postfix_try_can_consume_an_awaited_result_in_one_expression() {
    let verified = nexa_compiler::compile(
        r"
async fn produce() -> Result<i32, string> {
    yield;
    return Result::Ok(7);
}

async fn consume() -> Result<i32, string> {
    let value: i32 = produce().await?;
    return Result::Ok(value);
}
",
    )
    .expect("`.await?` must be accepted as one postfix chain");
    assert_eq!(verified.module().functions.len(), 2);
    assert!(
        verified.module().functions[1]
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::EnumTag { .. }))
    );
}

#[test]
fn await_outside_async_keeps_the_canonical_diagnostic_and_caller_span() {
    let source = r"
async fn produce() -> i32 {
    return 1;
}

fn consume() -> i32 {
    return produce().await;
}
";
    let error = nexa_compiler::compile(source).expect_err("sync functions cannot await");
    let CompileError::AnalysisDiagnostic(diagnostic) = error else {
        panic!("await rejection must come from canonical analysis");
    };
    assert_eq!(diagnostic.code, ErrorCode::NX2301);
    assert!(matches!(
        diagnostic.primary.source,
        AnalysisDiagnosticSource::Caller
    ));
    assert!(diagnostic.primary.span.start < diagnostic.primary.span.end);
    assert!(
        source[usize::try_from(diagnostic.primary.span.start).unwrap()
            ..usize::try_from(diagnostic.primary.span.end).unwrap()]
            .contains(".await")
    );
}

#[test]
fn async_calls_must_be_consumed_immediately() {
    let unconsumed = nexa_compiler::compile(
        r"
async fn produce() -> i32 {
    return 1;
}

async fn consume() -> i32 {
    let value: i32 = produce();
    return value;
}
",
    )
    .expect_err("an async result cannot be used without postfix await");
    let CompileError::AnalysisDiagnostic(diagnostic) = unconsumed else {
        panic!("missing await must remain a canonical analysis diagnostic");
    };
    assert_eq!(diagnostic.code, ErrorCode::NX2302);

    nexa_compiler::compile(
        r"
async fn produce() -> i32 {
    return 1;
}

async fn consume() -> i32 {
    let pending = produce();
    return pending.await;
}
",
    )
    .expect_err("awaitable temporaries are not a source-visible value type");
}

#[test]
fn prefix_await_is_not_part_of_language_v2() {
    nexa_compiler::compile(
        r"
async fn produce() -> i32 {
    return 1;
}

async fn consume() -> i32 {
    return await produce();
}
",
    )
    .expect_err("language v2 accepts only postfix `.await`");
}

#[test]
fn postfix_await_try_can_continue_into_field_and_index_postfixes() {
    let verified = nexa_compiler::compile(
        r"
struct Payload {
    value: i32,
}

async fn object() -> Result<Payload, string> {
    return Result::Ok(Payload { value: 3 });
}

async fn values() -> Result<Array<i32>, string> {
    return Result::Ok([5]);
}

async fn consume_object() -> Result<i32, string> {
    return Result::Ok(object().await?.value);
}

async fn consume_array() -> Result<i32, string> {
    return Result::Ok(values().await?[0]);
}
",
    )
    .expect("postfix await/try chains must remain available to later postfix operators");
    assert!(
        verified
            .module()
            .functions
            .iter()
            .flat_map(|function| &function.code)
            .any(|instruction| matches!(instruction, Instruction::StructGet { .. }))
    );
    assert!(
        verified
            .module()
            .functions
            .iter()
            .flat_map(|function| &function.code)
            .any(|instruction| matches!(instruction, Instruction::ArrayGet { .. }))
    );
}

#[test]
fn await_call_source_map_uses_the_outer_postfix_expression() {
    let source = r"
async fn produce() -> i32 {
    return 1;
}

async fn consume() -> i32 {
    return produce().await;
}
";
    let file = FileId(311);
    let verified = nexa_compiler::compile_file(source, file).expect("valid postfix await");
    let consume = 1_u32;
    let call_pc = verified.module().functions[usize::try_from(consume).unwrap()]
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Call { .. }))
        .expect("await lowers a call");
    let mapping = verified
        .module()
        .source_map
        .iter()
        .find(|entry| {
            entry.function == consume
                && entry.pc_start == u32::try_from(call_pc).unwrap()
                && entry.pc_end == u32::try_from(call_pc + 1).unwrap()
        })
        .expect("the awaited call has one exact source mapping");
    assert_eq!(mapping.span.file, file);
    assert_eq!(
        &source[usize::try_from(mapping.span.start).unwrap()
            ..usize::try_from(mapping.span.end).unwrap()],
        "produce().await"
    );
}

#[test]
fn await_is_rejected_inside_a_deferred_cleanup() {
    nexa_compiler::compile(
        r"
async fn produce() -> i32 {
    return 1;
}

async fn consume() -> i32 {
    defer produce().await;
    return 0;
}
",
    )
    .expect_err("deferred cleanup cannot suspend");
}

#[test]
fn host_request_is_not_a_source_visible_or_storable_type() {
    let error = nexa_compiler::compile(
        r"
fn forged(value: HostRequest<i32>) -> i32 {
    return 0;
}
",
    )
    .expect_err("HostRequest is an internal awaitable, not a Nexa type");
    assert!(matches!(error, CompileError::AnalysisDiagnostic(_)));
}
