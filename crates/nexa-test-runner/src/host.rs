use std::error::Error;
use std::fmt;

/// An opaque host invocation attempted by a compiled test backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCall {
    pub binding: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

impl HostCall {
    #[must_use]
    pub fn new(
        binding: impl Into<String>,
        operation: impl Into<String>,
        payload: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            binding: binding.into(),
            operation: operation.into(),
            payload: payload.into(),
        }
    }
}

/// Opaque response shape retained for backend adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostResponse {
    pub payload: Vec<u8>,
}

/// Host surface visible to a package-test backend.
pub trait TestHost {
    fn call(&mut self, call: HostCall) -> Result<HostResponse, RejectedHostCall>;
}

/// Defense-in-depth host used for every package test.
///
/// Eligibility should reject host reachability before execution. If invalid
/// bytecode or stale metadata nevertheless attempts a call, this host always
/// rejects it and never delegates to application code.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RejectingHost {
    attempts: usize,
    last_attempt: Option<HostCall>,
}

impl RejectingHost {
    #[must_use]
    pub const fn attempts(&self) -> usize {
        self.attempts
    }

    #[must_use]
    pub const fn last_attempt(&self) -> Option<&HostCall> {
        self.last_attempt.as_ref()
    }
}

impl TestHost for RejectingHost {
    fn call(&mut self, call: HostCall) -> Result<HostResponse, RejectedHostCall> {
        self.attempts = self.attempts.saturating_add(1);
        self.last_attempt = Some(call.clone());
        Err(RejectedHostCall { call })
    }
}

/// Deterministic rejection returned by [`RejectingHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectedHostCall {
    pub call: HostCall,
}

impl fmt::Display for RejectedHostCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "host call rejected in package test: {}::{}",
            self.call.binding, self.call.operation
        )
    }
}

impl Error for RejectedHostCall {}
