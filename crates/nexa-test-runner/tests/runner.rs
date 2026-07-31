use std::convert::Infallible;

use nexa_test_runner::{
    ExecutionReport, ExecutionTermination, HostCall, RejectingHost, SourceSpan, StackFrame,
    TestBackend, TestBackendFactory, TestDescriptor, TestError, TestHost, TestRunner, TestStatus,
};

#[derive(Clone, Copy, Debug)]
enum Function {
    True,
    False,
    Trap,
    FuelExhaustion,
    Isolation,
}

struct Backend {
    instance: usize,
}

impl TestBackend<Function> for Backend {
    fn execute(self, function: &Function, host: &mut RejectingHost) -> ExecutionReport {
        assert_eq!(host.attempts(), 0, "each test must receive a fresh host");
        let stack = vec![StackFrame::new(
            "example",
            "test.runner",
            format!("backend-{}", self.instance),
            None,
        )];

        match function {
            Function::True => {
                ExecutionReport::new(ExecutionTermination::Returned(true), stack, 11, 13)
            }
            Function::False => {
                ExecutionReport::new(ExecutionTermination::Returned(false), stack, 17, 19)
            }
            Function::Trap => ExecutionReport::new(
                ExecutionTermination::Trap {
                    message: "integer overflow".into(),
                },
                stack,
                23,
                29,
            ),
            Function::FuelExhaustion => {
                ExecutionReport::new(ExecutionTermination::FuelExhausted, stack, 31, 37)
            }
            Function::Isolation => {
                let rejection = host
                    .call(HostCall::new("business", "write", []))
                    .unwrap_err();
                assert_eq!(host.attempts(), 1);
                ExecutionReport::new(
                    ExecutionTermination::HostCallRejected(rejection),
                    stack,
                    41,
                    43,
                )
            }
        }
    }
}

#[derive(Default)]
struct Factory {
    created: usize,
}

impl TestBackendFactory<Function> for Factory {
    type Backend = Backend;
    type Error = Infallible;

    fn create(&mut self, _test: &TestDescriptor<Function>) -> Result<Self::Backend, Self::Error> {
        let instance = self.created;
        self.created += 1;
        Ok(Backend { instance })
    }
}

fn descriptor(name: &str, function: Function) -> TestDescriptor<Function> {
    TestDescriptor::new(
        "example",
        "test.runner",
        name,
        SourceSpan::new("tests/runner.nexa", 0, 1),
        function,
    )
}

#[test]
fn true_false_trap_and_fuel_exhaustion_have_fixed_classes_and_evidence() {
    let tests = [
        descriptor("d_fuel", Function::FuelExhaustion),
        descriptor("b_false", Function::False),
        descriptor("c_trap", Function::Trap),
        descriptor("a_true", Function::True),
    ];
    let mut factory = Factory::default();

    let run = TestRunner::run(tests, &mut factory);

    assert_eq!(factory.created, 4);
    assert_eq!(run.summary.passed, 1);
    assert_eq!(run.summary.failed, 1);
    assert_eq!(run.summary.errors, 2);
    assert_eq!(run.summary.total(), 4);
    assert!(!run.summary.is_success());

    assert_eq!(run.results[0].name, "a_true");
    assert_eq!(run.results[0].status, TestStatus::Pass);
    assert_eq!(run.results[0].instructions, 11);
    assert_eq!(run.results[0].fuel, 13);
    assert_eq!(run.results[0].stack[0].function, "backend-0");

    assert_eq!(run.results[1].name, "b_false");
    assert_eq!(run.results[1].status, TestStatus::Fail);
    assert_eq!(run.results[1].error, None);

    assert_eq!(run.results[2].name, "c_trap");
    assert_eq!(run.results[2].status, TestStatus::Error);
    assert_eq!(
        run.results[2].error,
        Some(TestError::Trap {
            message: "integer overflow".into()
        })
    );
    assert_eq!(run.results[2].span.source, "tests/runner.nexa");

    assert_eq!(run.results[3].name, "d_fuel");
    assert_eq!(run.results[3].error, Some(TestError::FuelExhaustion));
}

#[test]
fn backend_and_host_instances_are_isolated_per_test() {
    let mut factory = Factory::default();

    let run = TestRunner::run(
        [
            descriptor("first", Function::Isolation),
            descriptor("second", Function::Isolation),
        ],
        &mut factory,
    );

    assert_eq!(factory.created, 2);
    assert_eq!(run.results[0].stack[0].function, "backend-0");
    assert_eq!(run.results[1].stack[0].function, "backend-1");
    assert!(matches!(
        run.results[0].error,
        Some(TestError::HostCallRejected(_))
    ));
    assert!(matches!(
        run.results[1].error,
        Some(TestError::HostCallRejected(_))
    ));
}

#[test]
fn rejecting_host_never_delegates_or_returns_a_response() {
    let call = HostCall::new("inventory", "reserve", [1, 2, 3]);
    let mut host = RejectingHost::default();

    let rejection = host.call(call.clone()).unwrap_err();

    assert_eq!(rejection.call, call);
    assert_eq!(host.attempts(), 1);
    assert_eq!(host.last_attempt(), Some(&call));
    assert_eq!(
        rejection.to_string(),
        "host call rejected in package test: inventory::reserve"
    );
}
