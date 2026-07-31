use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;

/// A capability that package tests must not reach through their call graph.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ForbiddenEffect {
    Host,
    Task,
    Await,
    Yield,
    Activation,
    Migration,
    PersistentState,
}

/// Compiler-supplied call-graph and effect metadata for one function.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallGraphNode<F> {
    pub function: F,
    pub calls: Vec<F>,
    pub forbidden_effects: BTreeSet<ForbiddenEffect>,
}

impl<F> CallGraphNode<F> {
    #[must_use]
    pub fn new(function: F) -> Self {
        Self {
            function,
            calls: Vec::new(),
            forbidden_effects: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn with_calls(mut self, calls: impl IntoIterator<Item = F>) -> Self {
        self.calls.extend(calls);
        self
    }

    #[must_use]
    pub fn with_forbidden_effects(
        mut self,
        effects: impl IntoIterator<Item = ForbiddenEffect>,
    ) -> Self {
        self.forbidden_effects.extend(effects);
        self
    }
}

/// Deterministic function graph used for package-test eligibility checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallGraph<F> {
    nodes: BTreeMap<F, CallGraphNode<F>>,
}

impl<F: Ord + Clone> CallGraph<F> {
    pub fn new(
        nodes: impl IntoIterator<Item = CallGraphNode<F>>,
    ) -> Result<Self, CallGraphBuildError<F>> {
        let mut indexed = BTreeMap::new();

        for mut node in nodes {
            node.calls.sort();
            node.calls.dedup();
            let function = node.function.clone();
            if indexed.insert(function.clone(), node).is_some() {
                return Err(CallGraphBuildError::DuplicateFunction(function));
            }
        }

        Ok(Self { nodes: indexed })
    }

    #[must_use]
    pub fn node(&self, function: &F) -> Option<&CallGraphNode<F>> {
        self.nodes.get(function)
    }

    /// Validates all reachable functions. Breadth-first traversal plus sorted
    /// roots and edges produces one stable shortest path to each bad function.
    #[must_use]
    pub fn validate_tests(&self, tests: impl IntoIterator<Item = F>) -> EligibilityReport<F> {
        let mut roots: Vec<F> = tests.into_iter().collect();
        roots.sort();
        roots.dedup();

        let mut violations = Vec::new();
        for test in roots {
            self.validate_test(&test, &mut violations);
        }

        EligibilityReport { violations }
    }

    fn validate_test(&self, test: &F, violations: &mut Vec<EligibilityViolation<F>>) {
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::from([vec![test.clone()]]);

        while let Some(path) = queue.pop_front() {
            let function = path
                .last()
                .expect("eligibility traversal paths are never empty")
                .clone();
            if !visited.insert(function.clone()) {
                continue;
            }

            let Some(node) = self.nodes.get(&function) else {
                violations.push(EligibilityViolation {
                    test: test.clone(),
                    function,
                    path,
                    reason: EligibilityViolationReason::MissingMetadata,
                });
                continue;
            };

            for effect in &node.forbidden_effects {
                violations.push(EligibilityViolation {
                    test: test.clone(),
                    function: function.clone(),
                    path: path.clone(),
                    reason: EligibilityViolationReason::Forbidden(*effect),
                });
            }

            for called in &node.calls {
                if !visited.contains(called) {
                    let mut called_path = path.clone();
                    called_path.push(called.clone());
                    queue.push_back(called_path);
                }
            }
        }
    }
}

/// Structural graph metadata failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallGraphBuildError<F> {
    DuplicateFunction(F),
}

impl<F> fmt::Display for CallGraphBuildError<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateFunction(_) => {
                formatter.write_str("duplicate function in package-test call graph")
            }
        }
    }
}

impl<F: fmt::Debug> Error for CallGraphBuildError<F> {}

/// Why a reachable function made a test ineligible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EligibilityViolationReason {
    Forbidden(ForbiddenEffect),
    MissingMetadata,
}

/// One source-independent eligibility diagnostic with its stable call path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EligibilityViolation<F> {
    pub test: F,
    pub function: F,
    pub path: Vec<F>,
    pub reason: EligibilityViolationReason,
}

/// Aggregated result for all requested test roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EligibilityReport<F> {
    pub violations: Vec<EligibilityViolation<F>>,
}

impl<F> EligibilityReport<F> {
    #[must_use]
    pub fn is_eligible(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn into_result(self) -> Result<(), TestEligibilityError<F>> {
        if self.violations.is_empty() {
            Ok(())
        } else {
            Err(TestEligibilityError {
                violations: self.violations,
            })
        }
    }
}

/// Aggregated package-test call-graph rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestEligibilityError<F> {
    pub violations: Vec<EligibilityViolation<F>>,
}

impl<F> fmt::Display for TestEligibilityError<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} package-test eligibility violation(s)",
            self.violations.len()
        )
    }
}

impl<F: fmt::Debug> Error for TestEligibilityError<F> {}
