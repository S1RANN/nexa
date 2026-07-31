use std::fmt;

use crate::descriptor::{SourceSpan, TestDescriptor, compare_descriptors};
use crate::host::{RejectedHostCall, RejectingHost};

/// A source-addressable frame captured at termination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackFrame {
    pub package: String,
    pub module: String,
    pub function: String,
    pub span: Option<SourceSpan>,
}

impl StackFrame {
    #[must_use]
    pub fn new(
        package: impl Into<String>,
        module: impl Into<String>,
        function: impl Into<String>,
        span: Option<SourceSpan>,
    ) -> Self {
        Self {
            package: package.into(),
            module: module.into(),
            function: function.into(),
            span,
        }
    }
}

/// Backend termination before runner-level PASS/FAIL/ERROR classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionTermination {
    Returned(bool),
    Trap { message: String },
    FuelExhausted,
    HostCallRejected(RejectedHostCall),
    BackendError { message: String },
}

/// Execution evidence supplied by an interpreter or facade adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionReport {
    pub termination: ExecutionTermination,
    pub stack: Vec<StackFrame>,
    /// Number of bytecode instructions executed.
    pub instructions: u64,
    /// Fuel consumed by the test.
    pub fuel: u64,
}

impl ExecutionReport {
    #[must_use]
    pub fn new(
        termination: ExecutionTermination,
        stack: Vec<StackFrame>,
        instructions: u64,
        fuel: u64,
    ) -> Self {
        Self {
            termination,
            stack,
            instructions,
            fuel,
        }
    }

    #[must_use]
    pub fn returned(value: bool, instructions: u64, fuel: u64) -> Self {
        Self::new(
            ExecutionTermination::Returned(value),
            Vec::new(),
            instructions,
            fuel,
        )
    }
}

/// A single-use backend. Consuming `self` prevents one instance from running
/// more than one test.
pub trait TestBackend<F> {
    fn execute(self, function: &F, host: &mut RejectingHost) -> ExecutionReport;
}

/// Creates a fresh backend for every descriptor.
pub trait TestBackendFactory<F> {
    type Backend: TestBackend<F>;
    type Error: fmt::Display;

    fn create(&mut self, test: &TestDescriptor<F>) -> Result<Self::Backend, Self::Error>;
}

/// Stable, machine-readable result class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestStatus {
    Pass,
    Fail,
    Error,
}

impl TestStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Error => "ERROR",
        }
    }
}

impl fmt::Display for TestStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// ERROR detail. A boolean `false` is a FAIL and therefore has no error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestError {
    Trap { message: String },
    FuelExhaustion,
    HostCallRejected(RejectedHostCall),
    Backend { message: String },
    BackendSetup { message: String },
}

/// One deterministic package-test result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestResult {
    pub package: String,
    pub module: String,
    pub name: String,
    pub span: SourceSpan,
    pub status: TestStatus,
    pub error: Option<TestError>,
    pub stack: Vec<StackFrame>,
    pub instructions: u64,
    pub fuel: u64,
}

impl TestResult {
    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("{}::{}::{}", self.package, self.module, self.name)
    }
}

/// Aggregate counts derived from the result vector.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TestRunSummary {
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
}

impl TestRunSummary {
    #[must_use]
    pub const fn total(self) -> usize {
        self.passed + self.failed + self.errors
    }

    #[must_use]
    pub const fn is_success(self) -> bool {
        self.failed == 0 && self.errors == 0
    }
}

/// Deterministically ordered test results and their summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestRun {
    pub results: Vec<TestResult>,
    pub summary: TestRunSummary,
}

/// Stateless package-test runner.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TestRunner;

impl TestRunner {
    /// Sorts descriptors and executes each using a newly created backend and
    /// newly created rejecting host.
    pub fn run<F, Factory>(
        tests: impl IntoIterator<Item = TestDescriptor<F>>,
        factory: &mut Factory,
    ) -> TestRun
    where
        Factory: TestBackendFactory<F>,
    {
        let mut tests: Vec<_> = tests.into_iter().collect();
        tests.sort_by(compare_descriptors);

        let mut results = Vec::with_capacity(tests.len());
        for descriptor in tests {
            let report = match factory.create(&descriptor) {
                Ok(backend) => {
                    let mut host = RejectingHost::default();
                    backend.execute(&descriptor.function, &mut host)
                }
                Err(error) => {
                    results.push(setup_error_result(descriptor, error.to_string()));
                    continue;
                }
            };

            results.push(classify(descriptor, report));
        }

        let summary = summarize(&results);
        TestRun { results, summary }
    }
}

fn classify<F>(descriptor: TestDescriptor<F>, report: ExecutionReport) -> TestResult {
    let (status, error) = match report.termination {
        ExecutionTermination::Returned(true) => (TestStatus::Pass, None),
        ExecutionTermination::Returned(false) => (TestStatus::Fail, None),
        ExecutionTermination::Trap { message } => {
            (TestStatus::Error, Some(TestError::Trap { message }))
        }
        ExecutionTermination::FuelExhausted => (TestStatus::Error, Some(TestError::FuelExhaustion)),
        ExecutionTermination::HostCallRejected(rejection) => (
            TestStatus::Error,
            Some(TestError::HostCallRejected(rejection)),
        ),
        ExecutionTermination::BackendError { message } => {
            (TestStatus::Error, Some(TestError::Backend { message }))
        }
    };

    TestResult {
        package: descriptor.package,
        module: descriptor.module,
        name: descriptor.name,
        span: descriptor.span,
        status,
        error,
        stack: report.stack,
        instructions: report.instructions,
        fuel: report.fuel,
    }
}

fn setup_error_result<F>(descriptor: TestDescriptor<F>, message: String) -> TestResult {
    TestResult {
        package: descriptor.package,
        module: descriptor.module,
        name: descriptor.name,
        span: descriptor.span,
        status: TestStatus::Error,
        error: Some(TestError::BackendSetup { message }),
        stack: Vec::new(),
        instructions: 0,
        fuel: 0,
    }
}

fn summarize(results: &[TestResult]) -> TestRunSummary {
    let mut summary = TestRunSummary::default();
    for result in results {
        match result.status {
            TestStatus::Pass => summary.passed += 1,
            TestStatus::Fail => summary.failed += 1,
            TestStatus::Error => summary.errors += 1,
        }
    }
    summary
}
