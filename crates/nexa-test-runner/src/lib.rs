//! Pure package-test eligibility and execution abstractions.
//!
//! This crate deliberately does not depend on the compiler, bytecode, runtime,
//! embedding facade, or CLI. `nexa-analysis` is the sole authority for test
//! discovery and declaration validation; later layers translate analyzed test
//! identities and execution evidence into the small types exposed here.

mod descriptor;
mod eligibility;
mod host;
mod runner;

pub use descriptor::{SourceSpan, TestDescriptor};
pub use eligibility::{
    CallGraph, CallGraphBuildError, CallGraphNode, EligibilityReport, EligibilityViolation,
    EligibilityViolationReason, ForbiddenEffect, TestEligibilityError,
};
pub use host::{HostCall, HostResponse, RejectedHostCall, RejectingHost, TestHost};
pub use runner::{
    ExecutionReport, ExecutionTermination, StackFrame, TestBackend, TestBackendFactory, TestError,
    TestResult, TestRun, TestRunSummary, TestRunner, TestStatus,
};
