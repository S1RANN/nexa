//! Deterministic execution adapter for pure M4 package tests.
//!
//! Test compilation is intentionally separate from product compilation.  A
//! [`PackageTestArtifactRef`] points at an explicit test-only verified module;
//! callers must not attach it to a product artifact, Realm, or reload
//! candidate.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nexa_bytecode::{FunctionEffect, HostCallMode, HostImport, Instruction, ValueType};
use nexa_compiler::{
    PackageDebugInfo, PackageTestCallGraphNode, PackageTestForbiddenEffect, PackageTestInfo,
    PackageTestRejection,
};
use nexa_core::{FileId, SourceSpan as CoreSourceSpan, StableId};
use nexa_runtime::{
    CancelReason, HostCallOutcome, HostFunctionAuthority, HostFunctionSlot, HostRegistry, HostTrap,
    RealmConfig, RealmRuntime, ResolvedHostFunction, ResourceContext, RuntimeError, RuntimeHost,
    RuntimeHostArgs, RuntimeValue, StepConfig, TaskLimits, TaskPoll, TaskTerminalReason, Trap,
    TrapKind, YieldReason,
};
use nexa_test_runner::{
    CallGraph, CallGraphNode, EligibilityViolationReason, ExecutionReport, ExecutionTermination,
    ForbiddenEffect, HostCall, RejectingHost, SourceSpan, StackFrame, TestBackend,
    TestBackendFactory, TestDescriptor, TestHost, TestRun, TestRunner,
};
use nexa_verifier::VerifiedModule;

/// Fixed execution limits for one pure package test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackageTestOptions {
    /// Total fuel available before the result becomes `ERROR`.
    pub fuel_limit: u64,
}

impl Default for PackageTestOptions {
    fn default() -> Self {
        Self {
            fuel_limit: 100_000,
        }
    }
}

/// Exact package-relative source paths retained by an explicit test build.
///
/// The compiler owns `FileId` assignment inside one artifact.  This table
/// gives the runner stable, reader-facing paths without claiming that numeric
/// IDs remain stable across candidates.
pub(crate) type PackageTestSourcePaths = BTreeMap<FileId, String>;

/// Borrowed view of an explicit test-only artifact.
///
/// The verified module may contain production and dependency functions needed
/// by tests, plus `test.*` functions.  It is never the product runtime module:
/// test sources and test functions therefore remain outside product artifact
/// bytes and product fingerprints.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PackageTestArtifactRef<'a> {
    pub(crate) verified: &'a VerifiedModule,
    pub(crate) tests: &'a [PackageTestInfo],
    pub(crate) call_graph: &'a [PackageTestCallGraphNode],
    pub(crate) debug_info: &'a PackageDebugInfo,
    pub(crate) source_paths: &'a PackageTestSourcePaths,
}

/// One invalid `@test` declaration reported before any Realm is created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageTestDeclarationError {
    pub package: String,
    pub module: String,
    pub name: String,
    pub span: SourceSpan,
    pub reason: PackageTestDeclarationErrorReason,
}

/// Fixed M4 declaration contract for `@test`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageTestDeclarationErrorReason {
    ParametersMustBeEmpty,
    ResultMustBeBool,
    EffectMustBeImmediate,
    MissingFunction,
    MissingDebugFunction,
    MissingSource,
    SignatureMismatch,
    MetadataMismatch,
    ModuleMustBeTest,
    DuplicateQualifiedName,
    DuplicateFunction,
}

impl fmt::Display for PackageTestDeclarationErrorReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ParametersMustBeEmpty => "test parameters must be empty",
            Self::ResultMustBeBool => "test result must be bool",
            Self::EffectMustBeImmediate => "test execution must be immediate",
            Self::MissingFunction => "compiled test function is missing",
            Self::MissingDebugFunction => "compiled test function is missing debug metadata",
            Self::MissingSource => "compiled test definition source is missing",
            Self::SignatureMismatch => {
                "compiled test function must have zero parameters and return bool"
            }
            Self::MetadataMismatch => {
                "compiled test metadata does not match its function and debug records"
            }
            Self::ModuleMustBeTest => "test declaration must belong to a test.* module",
            Self::DuplicateQualifiedName => "duplicate package-test qualified name",
            Self::DuplicateFunction => "compiled function is registered as a test more than once",
        })
    }
}

/// Source-addressable function identity used by an eligibility call chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageTestFunctionLocation {
    pub stable_id: Option<StableId>,
    pub package: String,
    pub module: String,
    pub name: String,
    pub span: SourceSpan,
}

/// Stable reason that a reachable function cannot run in a pure package test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageTestEligibilityReason {
    Host,
    Task,
    Await,
    Yield,
    Activation,
    Migration,
    PersistentState,
    MissingMetadata,
}

impl fmt::Display for PackageTestEligibilityReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Host => "Host call",
            Self::Task => "Task",
            Self::Await => "await",
            Self::Yield => "yield",
            Self::Activation => "Activation",
            Self::Migration => "Migration",
            Self::PersistentState => "persistent State",
            Self::MissingMetadata => "missing call-graph metadata",
        })
    }
}

/// One source-addressable shortest call path from a test to a forbidden effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageTestEligibilityViolation {
    pub test: PackageTestFunctionLocation,
    pub function: PackageTestFunctionLocation,
    pub path: Vec<PackageTestFunctionLocation>,
    pub reason: PackageTestEligibilityReason,
}

/// Package-test setup or eligibility failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageTestRunError {
    InvalidOptions(&'static str),
    MissingHostContractRuntimeId,
    InvalidDeclarations(Vec<PackageTestDeclarationError>),
    ArtifactMetadataMismatch { function: Option<StableId> },
    InvalidCallGraph { function: Option<StableId> },
    CallGraphMetadataMismatch { function: Option<StableId> },
    Ineligible(Vec<PackageTestEligibilityViolation>),
}

impl fmt::Display for PackageTestRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOptions(message) => formatter.write_str(message),
            Self::MissingHostContractRuntimeId => {
                formatter.write_str("package-test artifact is missing its Host contract runtime ID")
            }
            Self::InvalidDeclarations(errors) => {
                write!(
                    formatter,
                    "{} invalid package-test declaration(s)",
                    errors.len()
                )?;
                if let Some(first) = errors.first() {
                    write!(
                        formatter,
                        ": {}::{}::{} at {}: {}",
                        first.package, first.module, first.name, first.span, first.reason
                    )?;
                }
                Ok(())
            }
            Self::ArtifactMetadataMismatch { function } => write!(
                formatter,
                "package-test debug/source metadata mismatch at stable function {function:?}"
            ),
            Self::InvalidCallGraph { function } => write!(
                formatter,
                "duplicate stable function in package-test call graph at {function:?}"
            ),
            Self::CallGraphMetadataMismatch { function } => {
                write!(
                    formatter,
                    "compiler package-test call-graph metadata mismatch at stable function \
                     {function:?}"
                )
            }
            Self::Ineligible(violations) => {
                write!(
                    formatter,
                    "{} package-test eligibility violation(s)",
                    violations.len()
                )?;
                if let Some(first) = violations.first() {
                    write!(
                        formatter,
                        ": {}::{}::{} reaches {}::{}::{} at {} via ",
                        first.test.package,
                        first.test.module,
                        first.test.name,
                        first.function.package,
                        first.function.module,
                        first.function.name,
                        first.function.span
                    )?;
                    for (index, location) in first.path.iter().enumerate() {
                        if index != 0 {
                            formatter.write_str(" -> ")?;
                        }
                        write!(
                            formatter,
                            "{}::{}::{}",
                            location.package, location.module, location.name
                        )?;
                    }
                    write!(formatter, ": {}", first.reason)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PackageTestRunError {}

/// Validates and executes all tests in deterministic qualified-name order.
///
/// Every descriptor gets a newly constructed Realm, heap, state registry,
/// scope, runtime host, and rejecting Host registry.  No Realm or state is
/// reused between tests.
pub(crate) fn run_package_tests(
    artifact: PackageTestArtifactRef<'_>,
    options: PackageTestOptions,
) -> Result<TestRun, PackageTestRunError> {
    validate_options(options)?;
    validate_runtime_metadata(artifact)?;
    let descriptors = descriptors(artifact)?;
    validate_eligibility(artifact, &descriptors)?;

    let mut factory = RealmTestBackendFactory {
        verified: artifact.verified,
        debug_info: artifact.debug_info,
        source_paths: artifact.source_paths,
        options,
        next_realm_id: 1,
    };
    Ok(TestRunner::run(descriptors, &mut factory))
}

/// Validates the test table, debug/source identity, compiler call graph, and
/// pure-test eligibility without creating a Realm or executing bytecode.
///
/// Test artifact constructors use this to reject stale or malformed compiler
/// output before publishing a [`PackageTestArtifactRef`].
pub(crate) fn validate_package_test_artifact(
    artifact: PackageTestArtifactRef<'_>,
) -> Result<(), PackageTestRunError> {
    validate_runtime_metadata(artifact)?;
    let descriptors = descriptors(artifact)?;
    validate_eligibility(artifact, &descriptors)
}

fn validate_runtime_metadata(
    artifact: PackageTestArtifactRef<'_>,
) -> Result<(), PackageTestRunError> {
    artifact
        .verified
        .module()
        .host_contract_id
        .ok_or(PackageTestRunError::MissingHostContractRuntimeId)
        .map(|_| ())
}

fn validate_options(options: PackageTestOptions) -> Result<(), PackageTestRunError> {
    if options.fuel_limit == 0 {
        return Err(PackageTestRunError::InvalidOptions(
            "package-test fuel limit must be non-zero",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn descriptors(
    artifact: PackageTestArtifactRef<'_>,
) -> Result<Vec<TestDescriptor<StableId>>, PackageTestRunError> {
    let module = artifact.verified.module();
    let mut declarations = Vec::new();
    let mut output = Vec::with_capacity(artifact.tests.len());

    let mut tests = artifact.tests.iter().collect::<Vec<_>>();
    tests.sort_by(|left, right| {
        (
            left.package_id.as_str(),
            left.module_path.as_str(),
            left.name.as_str(),
            left.definition_span.file,
            left.definition_span.start,
            left.definition_span.end,
            left.function_index,
            &left.canonical_identity,
            left.stable_id,
        )
            .cmp(&(
                right.package_id.as_str(),
                right.module_path.as_str(),
                right.name.as_str(),
                right.definition_span.file,
                right.definition_span.start,
                right.definition_span.end,
                right.function_index,
                &right.canonical_identity,
                right.stable_id,
            ))
    });
    let mut qualified_name_counts = BTreeMap::new();
    let mut function_index_counts = BTreeMap::new();
    for test in &tests {
        *qualified_name_counts
            .entry((
                test.package_id.as_str(),
                test.module_path.as_str(),
                test.name.as_str(),
            ))
            .or_insert(0_usize) += 1;
        *function_index_counts
            .entry(test.function_index)
            .or_insert(0_usize) += 1;
    }
    for test in tests {
        let span = runner_span(test.definition_span, artifact.source_paths);
        let function = module.functions.get(test.function_index as usize);
        let debug = artifact
            .debug_info
            .functions
            .iter()
            .find(|candidate| candidate.function_index == test.function_index);
        let reason = test
            .rejection
            .map(map_compiler_rejection)
            .or_else(|| {
                (!test.module_path.starts_with("test."))
                    .then_some(PackageTestDeclarationErrorReason::ModuleMustBeTest)
            })
            .or_else(|| {
                (test.package_id != artifact.debug_info.root_package_id
                    || test.canonical_identity.package_id() != test.package_id
                    || test.canonical_identity.module_path() != test.module_path
                    || test.canonical_identity.kind() != nexa_core::SymbolKind::Test
                    || test.canonical_identity.name() != test.name
                    || test.canonical_identity.explicit_stable_name().is_some()
                    || test.stable_id != test.canonical_identity.runtime_id())
                .then_some(PackageTestDeclarationErrorReason::MetadataMismatch)
            })
            .or_else(|| {
                (qualified_name_counts[&(
                    test.package_id.as_str(),
                    test.module_path.as_str(),
                    test.name.as_str(),
                )] > 1)
                    .then_some(PackageTestDeclarationErrorReason::DuplicateQualifiedName)
            })
            .or_else(|| {
                (function_index_counts[&test.function_index] > 1)
                    .then_some(PackageTestDeclarationErrorReason::DuplicateFunction)
            })
            .or_else(|| {
                (!artifact
                    .source_paths
                    .contains_key(&test.definition_span.file))
                .then_some(PackageTestDeclarationErrorReason::MissingSource)
            })
            .or_else(|| {
                let function = function?;
                (!function.signature.parameters.is_empty()
                    || function.signature.result != Some(ValueType::Bool))
                .then_some(PackageTestDeclarationErrorReason::SignatureMismatch)
            })
            .or_else(|| {
                function
                    .is_none()
                    .then_some(PackageTestDeclarationErrorReason::MissingFunction)
            })
            .or_else(|| {
                debug
                    .is_none()
                    .then_some(PackageTestDeclarationErrorReason::MissingDebugFunction)
            })
            .or_else(|| {
                let function = function?;
                let debug = debug?;
                let export_matches = module
                    .exports
                    .iter()
                    .filter(|export| {
                        export.stable_id == test.stable_id.0
                            && export.function == test.function_index
                            && export.signature == function.signature
                            && export.effect == function.effect
                    })
                    .count()
                    == 1;
                (test.effect != function.effect
                    || test.effect != debug.effect
                    || test.package_id != debug.package_id
                    || test.module_path != debug.module_path
                    || test.name != debug.name
                    || test.definition_span != debug.definition_span
                    || test.stable_id != debug.stable_id
                    || test.canonical_identity != debug.canonical_identity
                    || !export_matches)
                    .then_some(PackageTestDeclarationErrorReason::MetadataMismatch)
            })
            .or_else(|| {
                (test.effect != FunctionEffect::Immediate)
                    .then_some(PackageTestDeclarationErrorReason::EffectMustBeImmediate)
            });

        if let Some(reason) = reason {
            declarations.push(PackageTestDeclarationError {
                package: test.package_id.clone(),
                module: test.module_path.clone(),
                name: test.name.clone(),
                span,
                reason,
            });
            continue;
        }

        output.push(TestDescriptor::new(
            test.package_id.clone(),
            test.module_path.clone(),
            test.name.clone(),
            span,
            test.stable_id.0,
        ));
    }

    declarations.sort_by(|left, right| {
        (
            left.package.as_str(),
            left.module.as_str(),
            left.name.as_str(),
            &left.span,
        )
            .cmp(&(
                right.package.as_str(),
                right.module.as_str(),
                right.name.as_str(),
                &right.span,
            ))
    });
    if declarations.is_empty() {
        Ok(output)
    } else {
        Err(PackageTestRunError::InvalidDeclarations(declarations))
    }
}

const fn map_compiler_rejection(
    rejection: PackageTestRejection,
) -> PackageTestDeclarationErrorReason {
    match rejection {
        PackageTestRejection::ParametersMustBeEmpty => {
            PackageTestDeclarationErrorReason::ParametersMustBeEmpty
        }
        PackageTestRejection::ResultMustBeBool => {
            PackageTestDeclarationErrorReason::ResultMustBeBool
        }
        PackageTestRejection::EffectMustBeImmediate => {
            PackageTestDeclarationErrorReason::EffectMustBeImmediate
        }
    }
}

#[allow(clippy::too_many_lines)]
fn validate_eligibility(
    artifact: PackageTestArtifactRef<'_>,
    tests: &[TestDescriptor<StableId>],
) -> Result<(), PackageTestRunError> {
    let expected = bytecode_call_graph_nodes(artifact.verified.module());
    let test_functions = artifact
        .tests
        .iter()
        .map(|test| test.function_index)
        .collect::<BTreeSet<_>>();
    let mut canonical_identities = BTreeSet::new();
    let mut runtime_identities = BTreeMap::new();
    let compiler_nodes = artifact
        .call_graph
        .iter()
        .map(|node| {
            CallGraphNode::new(node.function_index)
                .with_calls(node.calls.iter().copied())
                .with_forbidden_effects(
                    node.forbidden_effects
                        .iter()
                        .copied()
                        .map(map_compiler_forbidden_effect),
                )
        })
        .collect::<Vec<_>>();
    let raw_graph = CallGraph::new(compiler_nodes).map_err(|error| {
        let nexa_test_runner::CallGraphBuildError::DuplicateFunction(function) = error;
        PackageTestRunError::InvalidCallGraph {
            function: stable_function_id(function, artifact.debug_info),
        }
    })?;
    if artifact.call_graph.len() != expected.len() {
        return Err(PackageTestRunError::CallGraphMetadataMismatch { function: None });
    }
    for expected_node in &expected {
        let debug = artifact
            .debug_info
            .functions
            .iter()
            .filter(|function| function.function_index == expected_node.function)
            .collect::<Vec<_>>();
        if debug.len() != 1
            || !artifact
                .source_paths
                .contains_key(&debug[0].definition_span.file)
        {
            return Err(PackageTestRunError::ArtifactMetadataMismatch {
                function: stable_function_id(expected_node.function, artifact.debug_info),
            });
        }
        let function = &artifact.verified.module().functions[expected_node.function as usize];
        let debug = debug[0];
        let expected_kind = if test_functions.contains(&expected_node.function) {
            nexa_core::SymbolKind::Test
        } else if function.effect == FunctionEffect::Task {
            nexa_core::SymbolKind::Task
        } else {
            nexa_core::SymbolKind::Function
        };
        let canonical = &debug.canonical_identity;
        let identity_shape_is_valid =
            canonical_debug_package_matches(canonical.package_id(), &debug.package_id)
                && canonical.kind() == expected_kind
                && debug.stable_id == canonical.runtime_id()
                && canonical.explicit_stable_name().map_or_else(
                    || {
                        canonical.module_path() == debug.module_path
                            && canonical.name() == debug.name
                    },
                    |stable_name| {
                        canonical.module_path().is_empty()
                            && canonical.name() == stable_name
                            && valid_explicit_stable_name(stable_name)
                    },
                );
        if debug.effect != function.effect
            || !identity_shape_is_valid
            || !canonical_identities.insert(canonical.clone())
            || runtime_identities
                .insert(debug.stable_id, canonical.clone())
                .is_some_and(|prior| &prior != canonical)
        {
            return Err(PackageTestRunError::ArtifactMetadataMismatch {
                function: stable_function_id(expected_node.function, artifact.debug_info),
            });
        }
        let compiler_node = raw_graph.node(&expected_node.function);
        if compiler_node.is_none_or(|compiler_node| {
            compiler_node.calls != expected_node.calls
                || !compiler_node
                    .forbidden_effects
                    .is_superset(&expected_node.forbidden_effects)
        }) {
            return Err(PackageTestRunError::CallGraphMetadataMismatch {
                function: stable_function_id(expected_node.function, artifact.debug_info),
            });
        }
    }

    let stable_nodes = artifact
        .call_graph
        .iter()
        .map(|node| {
            let function = stable_function_id(node.function_index, artifact.debug_info)
                .ok_or(PackageTestRunError::ArtifactMetadataMismatch { function: None })?;
            let calls = node
                .calls
                .iter()
                .map(|call| {
                    stable_function_id(*call, artifact.debug_info)
                        .ok_or(PackageTestRunError::ArtifactMetadataMismatch { function: None })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CallGraphNode::new(function)
                .with_calls(calls)
                .with_forbidden_effects(
                    node.forbidden_effects
                        .iter()
                        .copied()
                        .map(map_compiler_forbidden_effect),
                ))
        })
        .collect::<Result<Vec<_>, PackageTestRunError>>()?;
    let stable_graph = CallGraph::new(stable_nodes).map_err(|error| {
        let nexa_test_runner::CallGraphBuildError::DuplicateFunction(function) = error;
        PackageTestRunError::InvalidCallGraph {
            function: Some(function),
        }
    })?;
    let report = stable_graph.validate_tests(tests.iter().map(|test| test.function));
    if report.violations.is_empty() {
        return Ok(());
    }
    let violations = report
        .violations
        .into_iter()
        .map(|violation| {
            let reason = map_eligibility_reason(&violation.reason);
            PackageTestEligibilityViolation {
                test: function_location(violation.test, artifact.debug_info, artifact.source_paths),
                function: function_location(
                    violation.function,
                    artifact.debug_info,
                    artifact.source_paths,
                ),
                path: violation
                    .path
                    .into_iter()
                    .map(|function| {
                        function_location(function, artifact.debug_info, artifact.source_paths)
                    })
                    .collect(),
                reason,
            }
        })
        .collect();
    Err(PackageTestRunError::Ineligible(violations))
}

fn valid_explicit_stable_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && name.len() <= 128
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn bytecode_call_graph_nodes(module: &nexa_bytecode::Module) -> Vec<CallGraphNode<u32>> {
    let mut nodes = module
        .functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let mut calls = Vec::new();
            let mut effects = BTreeSet::new();
            match function.effect {
                FunctionEffect::Task => {
                    effects.insert(ForbiddenEffect::Task);
                }
                FunctionEffect::Migration | FunctionEffect::Cleanup => {
                    effects.insert(ForbiddenEffect::Migration);
                }
                FunctionEffect::Ordinary | FunctionEffect::Immediate => {}
            }
            if module.reload_metadata.activation_entry == Some(index) {
                effects.insert(ForbiddenEffect::Activation);
            }
            if module.reload_metadata.migration_entry == Some(index) {
                effects.insert(ForbiddenEffect::Migration);
            }

            for instruction in &function.code {
                match instruction {
                    Instruction::Call {
                        function: callee, ..
                    }
                    | Instruction::DeferPush {
                        function: callee, ..
                    } => calls.push(*callee),
                    Instruction::HostCall { import, .. } => {
                        effects.insert(ForbiddenEffect::Host);
                        if module
                            .host_imports
                            .get(*import as usize)
                            .is_some_and(|import| import.mode == HostCallMode::Async)
                        {
                            effects.insert(ForbiddenEffect::Await);
                        }
                    }
                    Instruction::Yield => {
                        effects.insert(ForbiddenEffect::Yield);
                    }
                    instruction if uses_persistent_state(instruction) => {
                        effects.insert(ForbiddenEffect::PersistentState);
                    }
                    _ => {}
                }
            }
            CallGraphNode::new(index)
                .with_calls(calls)
                .with_forbidden_effects(effects)
        })
        .collect::<Vec<_>>();
    for node in &mut nodes {
        node.calls.sort_unstable();
        node.calls.dedup();
    }
    nodes
}

const fn map_compiler_forbidden_effect(effect: PackageTestForbiddenEffect) -> ForbiddenEffect {
    match effect {
        PackageTestForbiddenEffect::Host => ForbiddenEffect::Host,
        PackageTestForbiddenEffect::Task => ForbiddenEffect::Task,
        PackageTestForbiddenEffect::Await => ForbiddenEffect::Await,
        PackageTestForbiddenEffect::Yield => ForbiddenEffect::Yield,
        PackageTestForbiddenEffect::Activation => ForbiddenEffect::Activation,
        PackageTestForbiddenEffect::Migration => ForbiddenEffect::Migration,
        PackageTestForbiddenEffect::PersistentState => ForbiddenEffect::PersistentState,
    }
}

fn function_location(
    stable_id: StableId,
    debug_info: &PackageDebugInfo,
    source_paths: &PackageTestSourcePaths,
) -> PackageTestFunctionLocation {
    let function = debug_info
        .functions
        .iter()
        .find(|function| function.stable_id.0 == stable_id);
    PackageTestFunctionLocation {
        stable_id: Some(stable_id),
        package: function.map_or_else(
            || "<unknown-package>".into(),
            |function| function.package_id.clone(),
        ),
        module: function.map_or_else(
            || "<unknown-module>".into(),
            |function| function.module_path.clone(),
        ),
        name: function.map_or_else(
            || "<unknown-function>".into(),
            |function| function.name.clone(),
        ),
        span: function.map_or_else(
            || SourceSpan::new("<unknown-source>", 0, 0),
            |function| runner_span(function.definition_span, source_paths),
        ),
    }
}

fn stable_function_id(function_index: u32, debug_info: &PackageDebugInfo) -> Option<StableId> {
    debug_info
        .functions
        .iter()
        .find(|function| function.function_index == function_index)
        .map(|function| function.stable_id.0)
}

const fn map_eligibility_reason(
    reason: &EligibilityViolationReason,
) -> PackageTestEligibilityReason {
    match reason {
        EligibilityViolationReason::Forbidden(ForbiddenEffect::Host) => {
            PackageTestEligibilityReason::Host
        }
        EligibilityViolationReason::Forbidden(ForbiddenEffect::Task) => {
            PackageTestEligibilityReason::Task
        }
        EligibilityViolationReason::Forbidden(ForbiddenEffect::Await) => {
            PackageTestEligibilityReason::Await
        }
        EligibilityViolationReason::Forbidden(ForbiddenEffect::Yield) => {
            PackageTestEligibilityReason::Yield
        }
        EligibilityViolationReason::Forbidden(ForbiddenEffect::Activation) => {
            PackageTestEligibilityReason::Activation
        }
        EligibilityViolationReason::Forbidden(ForbiddenEffect::Migration) => {
            PackageTestEligibilityReason::Migration
        }
        EligibilityViolationReason::Forbidden(ForbiddenEffect::PersistentState) => {
            PackageTestEligibilityReason::PersistentState
        }
        EligibilityViolationReason::MissingMetadata => {
            PackageTestEligibilityReason::MissingMetadata
        }
    }
}

const fn uses_persistent_state(instruction: &Instruction) -> bool {
    matches!(
        instruction,
        Instruction::StateOldGet { .. }
            | Instruction::StateNewCreate { .. }
            | Instruction::StateNewSet { .. }
            | Instruction::StateReplace { .. }
            | Instruction::StatePreserve { .. }
            | Instruction::StateDelete { .. }
            | Instruction::StateFinish
            | Instruction::StateOldFieldGet { .. }
            | Instruction::StateHandleResolve { .. }
            | Instruction::StateHandleIsAlive { .. }
            | Instruction::StateHandleStableId { .. }
            | Instruction::StateHandleGeneration { .. }
            | Instruction::StateHandleEqual { .. }
            | Instruction::StateHandleHash { .. }
    )
}

fn runner_span(span: CoreSourceSpan, paths: &PackageTestSourcePaths) -> SourceSpan {
    SourceSpan::new(source_path(span.file, paths), span.start, span.end)
}

fn source_path(file: FileId, paths: &PackageTestSourcePaths) -> String {
    paths
        .get(&file)
        .cloned()
        .unwrap_or_else(|| format!("<file #{}>", file.0))
}

struct RealmTestBackendFactory<'a> {
    verified: &'a VerifiedModule,
    debug_info: &'a PackageDebugInfo,
    source_paths: &'a PackageTestSourcePaths,
    options: PackageTestOptions,
    next_realm_id: u32,
}

impl<'a> TestBackendFactory<StableId> for RealmTestBackendFactory<'a> {
    type Backend = RealmTestBackend<'a>;
    type Error = PackageTestBackendSetupError;

    fn create(&mut self, _test: &TestDescriptor<StableId>) -> Result<Self::Backend, Self::Error> {
        let realm_id = self.next_realm_id;
        self.next_realm_id = self
            .next_realm_id
            .checked_add(1)
            .ok_or(PackageTestBackendSetupError::RealmIdExhausted)?;

        let module = self.verified.module();
        let host_hash = module
            .host_contract_id
            .ok_or(PackageTestBackendSetupError::MissingHostHash)?;
        let state_schema_fingerprint = module.state_schema_fingerprint;

        let runtime_host = RuntimeHost::new(8);
        let config = RealmConfig {
            realm_id,
            max_modules: 1,
            ..RealmConfig::default()
        };
        let mut realm = match RealmRuntime::hosted(
            config,
            runtime_host.clone(),
            Box::new(RejectingRuntimeHost::new(host_hash, &module.host_imports)),
        ) {
            Ok(realm) => realm,
            Err(error) => {
                close_runtime_host(&runtime_host);
                return Err(PackageTestBackendSetupError::Realm(error));
            }
        };
        let module_handle =
            match realm.load_module(self.verified.clone(), host_hash, state_schema_fingerprint) {
                Ok(module) => module,
                Err(error) => {
                    drop(realm);
                    close_runtime_host(&runtime_host);
                    return Err(PackageTestBackendSetupError::Realm(error));
                }
            };

        Ok(RealmTestBackend {
            realm,
            module_handle,
            runtime_host,
            options: self.options,
            debug_info: self.debug_info,
            source_paths: self.source_paths,
        })
    }
}

#[derive(Debug)]
pub enum PackageTestBackendSetupError {
    RealmIdExhausted,
    MissingHostHash,
    Realm(nexa_runtime::RealmError),
}

impl fmt::Display for PackageTestBackendSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RealmIdExhausted => formatter.write_str("package-test Realm IDs exhausted"),
            Self::MissingHostHash => {
                formatter.write_str("package-test module is missing its Host contract hash")
            }
            Self::Realm(error) => write!(formatter, "package-test Realm setup failed: {error}"),
        }
    }
}

impl std::error::Error for PackageTestBackendSetupError {}

struct RealmTestBackend<'a> {
    realm: RealmRuntime,
    module_handle: nexa_runtime::ModuleHandle,
    runtime_host: RuntimeHost,
    options: PackageTestOptions,
    debug_info: &'a PackageDebugInfo,
    source_paths: &'a PackageTestSourcePaths,
}

impl TestBackend<StableId> for RealmTestBackend<'_> {
    #[allow(clippy::too_many_lines)]
    fn execute(self, function: &StableId, host: &mut RejectingHost) -> ExecutionReport {
        let Self {
            mut realm,
            module_handle,
            runtime_host,
            options,
            debug_info,
            source_paths,
        } = self;

        let scope = match realm.create_scope(None) {
            Ok(scope) => scope,
            Err(error) => {
                return finish_backend(
                    realm,
                    &runtime_host,
                    ExecutionReport::new(
                        ExecutionTermination::BackendError {
                            message: format!("failed to create package-test scope: {error}"),
                        },
                        Vec::new(),
                        0,
                        0,
                    ),
                );
            }
        };
        let task = match realm.spawn_task(
            module_handle,
            *function,
            &[],
            StepConfig {
                owner: scope,
                priority: 0,
                fuel_slice: options.fuel_limit,
                cumulative_budget: options.fuel_limit,
                limits: TaskLimits::default(),
            },
        ) {
            Ok(task) => task,
            Err(error) => {
                return finish_backend(
                    realm,
                    &runtime_host,
                    ExecutionReport::new(
                        ExecutionTermination::BackendError {
                            message: format!("failed to start package test: {error}"),
                        },
                        Vec::new(),
                        0,
                        0,
                    ),
                );
            }
        };

        let mut termination = loop {
            match realm.poll_task(task, options.fuel_limit) {
                Ok(TaskPoll::Completed(RuntimeValue::Bool(value))) => {
                    break ExecutionTermination::Returned(value);
                }
                Ok(TaskPoll::Completed(value)) => {
                    break ExecutionTermination::BackendError {
                        message: format!("package test returned non-bool runtime value: {value:?}"),
                    };
                }
                Ok(TaskPoll::Yielded(YieldReason::Fuel)) => {}
                Ok(TaskPoll::Yielded(YieldReason::Explicit)) => {
                    let _ = realm.cancel_scope(scope);
                    break ExecutionTermination::BackendError {
                        message: "package test yielded explicitly".into(),
                    };
                }
                Ok(TaskPoll::Waiting(_)) => {
                    let _ = realm.cancel_scope(scope);
                    break reject_host_call(host, "async-host-call");
                }
                Ok(TaskPoll::Cancelled(CancelReason::BudgetExceeded)) => {
                    break ExecutionTermination::FuelExhausted;
                }
                Ok(TaskPoll::Cancelled(reason)) => {
                    break ExecutionTermination::BackendError {
                        message: format!("package test was cancelled: {reason:?}"),
                    };
                }
                Ok(TaskPoll::Trapped(_)) => {
                    let trap = terminal_trap(&realm, task);
                    break match trap {
                        Some(trap) if trap.kind == TrapKind::Host => {
                            reject_runtime_host_call(host, trap)
                        }
                        Some(trap) => ExecutionTermination::Trap {
                            message: trap.message.to_string(),
                        },
                        None => ExecutionTermination::BackendError {
                            message: "package-test trap is missing its terminal record".into(),
                        },
                    };
                }
                Err(RuntimeError::TerminalTask) => {
                    break ExecutionTermination::BackendError {
                        message: "package-test task became terminal without a result".into(),
                    };
                }
                Err(error) => {
                    break ExecutionTermination::BackendError {
                        message: format!("package-test runtime failed: {error}"),
                    };
                }
            }
        };

        let terminal = realm.terminal_record(task);
        let (instructions, fuel, stack) = if let Some(record) = terminal {
            let stack = terminal_stack(record, debug_info, source_paths);
            match stack {
                Ok(stack) => (
                    record.final_charge.instructions,
                    record.final_charge.fuel_used,
                    stack,
                ),
                Err(message) => {
                    termination = ExecutionTermination::BackendError {
                        message: message.into(),
                    };
                    (
                        record.final_charge.instructions,
                        record.final_charge.fuel_used,
                        Vec::new(),
                    )
                }
            }
        } else {
            termination = ExecutionTermination::BackendError {
                message: "package-test task is missing its terminal execution record".into(),
            };
            (0, 0, Vec::new())
        };
        if matches!(
            &termination,
            ExecutionTermination::Trap { .. }
                | ExecutionTermination::FuelExhausted
                | ExecutionTermination::HostCallRejected(_)
        ) && stack.is_empty()
        {
            termination = ExecutionTermination::BackendError {
                message: "package-test terminal is missing its exact script call stack".into(),
            };
        }
        finish_backend(
            realm,
            &runtime_host,
            ExecutionReport::new(termination, stack, instructions, fuel),
        )
    }
}

fn reject_runtime_host_call(host: &mut RejectingHost, trap: &Trap) -> ExecutionTermination {
    let operation = trap.host_call_boundary.map_or_else(
        || "unknown".into(),
        |boundary| format!("import-{}", boundary.import),
    );
    reject_host_call(host, &operation)
}

fn reject_host_call(host: &mut RejectingHost, operation: &str) -> ExecutionTermination {
    match host.call(HostCall::new("host", operation, [])) {
        Ok(_) => ExecutionTermination::BackendError {
            message: "rejecting package-test Host unexpectedly accepted a call".into(),
        },
        Err(rejection) => ExecutionTermination::HostCallRejected(rejection),
    }
}

fn terminal_trap(realm: &RealmRuntime, task: nexa_runtime::TaskHandle) -> Option<&Trap> {
    let terminal = realm.terminal_record(task)?;
    match &terminal.reason {
        TaskTerminalReason::Trapped(trap) => Some(trap),
        TaskTerminalReason::Completed(_) | TaskTerminalReason::Cancelled(_) => None,
    }
}

fn canonical_debug_package_matches(canonical: &str, source_package: &str) -> bool {
    canonical == source_package
        || (source_package == nexa_stdlib::PACKAGE_ID
            && canonical == nexa_stdlib::CANONICAL_PACKAGE_ID)
}

fn terminal_stack(
    terminal: &nexa_runtime::TaskTerminalRecord,
    debug_info: &PackageDebugInfo,
    source_paths: &PackageTestSourcePaths,
) -> Result<Vec<StackFrame>, &'static str> {
    let script_call_stack = terminal.script_call_stack.as_ref().or_else(|| {
        let TaskTerminalReason::Trapped(trap) = &terminal.reason else {
            return None;
        };
        Some(&trap.script_call_stack)
    });
    script_call_stack
        .map_or(&[][..], nexa_runtime::ScriptCallStack::as_slice)
        .iter()
        .map(|frame| {
            let function = debug_info
                .functions
                .iter()
                .find(|candidate| candidate.function_index == frame.function)
                .ok_or("package-test terminal stack references missing function debug metadata")?;
            let span = frame
                .source_span
                .ok_or("package-test terminal stack frame is missing an exact source span")?;
            let path = source_paths
                .get(&span.file)
                .ok_or("package-test terminal stack frame references an unknown source file")?;
            Ok(StackFrame::new(
                function.package_id.as_str(),
                function.module_path.as_str(),
                function.name.as_str(),
                Some(SourceSpan::new(path.as_str(), span.start, span.end)),
            ))
        })
        .collect()
}

fn finish_backend(
    realm: RealmRuntime,
    runtime_host: &RuntimeHost,
    mut report: ExecutionReport,
) -> ExecutionReport {
    drop(realm);
    let _ = runtime_host.begin_close();
    if let Err(error) = runtime_host.try_finish_close() {
        report.termination = ExecutionTermination::BackendError {
            message: format!("package-test Realm shutdown failed: {error}"),
        };
    }
    report
}

fn close_runtime_host(runtime_host: &RuntimeHost) {
    let _ = runtime_host.begin_close();
    let _ = runtime_host.try_finish_close();
}

struct RejectingRuntimeHost {
    contract_runtime_id: StableId,
    authorities: Vec<HostFunctionAuthority>,
}

impl RejectingRuntimeHost {
    fn new(contract_runtime_id: StableId, imports: &[HostImport]) -> Self {
        Self {
            contract_runtime_id,
            authorities: imports
                .iter()
                .map(HostFunctionAuthority::from_import)
                .collect(),
        }
    }
}

impl HostRegistry for RejectingRuntimeHost {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.contract_runtime_id)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        self.authorities
            .iter()
            .enumerate()
            .find(|(_, authority)| authority.stable_id() == id)
            .and_then(|(index, authority)| {
                u32::try_from(index)
                    .ok()
                    .map(|index| ResolvedHostFunction::new(HostFunctionSlot::new(index), authority))
            })
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        _context: &mut ResourceContext<'_>,
        _args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        let Some(authority) = self.authorities.get(slot.index() as usize) else {
            return Err(HostTrap::InvalidFunctionSlot(slot));
        };
        let id = authority.stable_id();
        Err(HostTrap::UnknownFunction(id))
    }
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{
        Function, FunctionBuilder, HostImport, ModuleBuilder, ScriptExport, Signature,
        SourceMapEntry, StandardIntrinsic,
    };
    use nexa_compiler::{PackageFunctionDebugInfo, PackageVisibility};
    use nexa_core::{CanonicalSymbolIdentity, SymbolKind};
    use nexa_test_runner::{ForbiddenEffect, TestError, TestStatus};
    use nexa_verifier::{VerifierLimits, verify};

    use super::*;

    const FILE: FileId = FileId(1);

    fn bool_function(value: bool) -> Function {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Bool),
            },
            1,
        );
        function
            .emit(Instruction::LoadBool { dst: 0, value })
            .emit(Instruction::Return { source: 0 });
        function.finish().unwrap()
    }

    fn trap_function() -> Function {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Bool),
            },
            1,
        );
        function.emit(Instruction::Trap);
        function.finish().unwrap()
    }

    fn call_function(callee: u32) -> Function {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Bool),
            },
            1,
        );
        function
            .emit(Instruction::Call {
                function: callee,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        function.finish().unwrap()
    }

    fn expensive_intrinsic_function() -> Function {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Bool),
            },
            3,
        );
        function
            .emit(Instruction::LoadF64 {
                dst: 0,
                bits: 0.0_f64.to_bits(),
            })
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::F64Sin,
                args_base: 0,
                args_count: 1,
                dst: 1,
            })
            .emit(Instruction::LoadBool {
                dst: 2,
                value: true,
            })
            .emit(Instruction::Return { source: 2 });
        function.finish().unwrap()
    }

    fn test_info(index: u32, name: &str, span: CoreSourceSpan) -> PackageTestInfo {
        let identity = CanonicalSymbolIdentity::automatic(
            "example.tests",
            "test.sample",
            SymbolKind::Test,
            name,
        );
        PackageTestInfo {
            package_id: "example.tests".into(),
            module_path: "test.sample".into(),
            name: name.into(),
            function_index: index,
            stable_id: identity.runtime_id(),
            canonical_identity: identity,
            definition_span: span,
            effect: FunctionEffect::Immediate,
            rejection: None,
        }
    }

    fn debug_function(
        index: u32,
        name: &str,
        span: CoreSourceSpan,
        kind: SymbolKind,
        effect: FunctionEffect,
    ) -> PackageFunctionDebugInfo {
        let identity =
            CanonicalSymbolIdentity::automatic("example.tests", "test.sample", kind, name);
        PackageFunctionDebugInfo {
            function_index: index,
            package_id: "example.tests".into(),
            module_path: "test.sample".into(),
            name: name.into(),
            stable_id: identity.runtime_id(),
            canonical_identity: identity,
            definition_span: span,
            effect,
            visibility: PackageVisibility::Private,
        }
    }

    fn build_artifact(
        functions: Vec<(String, Function, CoreSourceSpan)>,
        tests: Vec<PackageTestInfo>,
        configure: impl FnOnce(&mut ModuleBuilder),
    ) -> (
        VerifiedModule,
        Vec<PackageTestInfo>,
        Vec<PackageTestCallGraphNode>,
        PackageDebugInfo,
        PackageTestSourcePaths,
    ) {
        let host = StableId::from_name("test-host");
        let state_schema_fingerprint = nexa_bytecode::StateSchema::default().fingerprint();
        let mut builder = ModuleBuilder::new();
        builder.metadata(host, state_schema_fingerprint);
        configure(&mut builder);
        let mut source_map = Vec::new();
        let mut debug = Vec::new();
        let test_indices = tests
            .iter()
            .map(|test| test.function_index)
            .collect::<BTreeSet<_>>();
        let test_exports = tests
            .iter()
            .filter(|test| test.rejection.is_none())
            .map(|test| (test.function_index, test.stable_id.0))
            .collect::<BTreeMap<_, _>>();
        let mut emitted_export_ids = BTreeSet::new();
        for (index, (name, mut function, span)) in functions.into_iter().enumerate() {
            let index = u32::try_from(index).unwrap();
            let effect = FunctionEffect::Immediate;
            function.effect = effect;
            let signature = function.signature.clone();
            let end = u32::try_from(function.code.len()).unwrap();
            builder.function(function);
            if let Some(stable_id) = test_exports.get(&index)
                && emitted_export_ids.insert(*stable_id)
            {
                builder.script_export(ScriptExport {
                    stable_id: *stable_id,
                    function: index,
                    signature,
                    effect,
                });
            }
            source_map.push(SourceMapEntry {
                function: index,
                pc_start: 0,
                pc_end: end,
                span,
            });
            debug.push(debug_function(
                index,
                &name,
                span,
                if test_indices.contains(&index) {
                    SymbolKind::Test
                } else {
                    SymbolKind::Function
                },
                effect,
            ));
        }
        builder.source_map(source_map);
        let verified = verify(builder.finish(), VerifierLimits::default()).unwrap();
        let call_graph = bytecode_call_graph_nodes(verified.module())
            .into_iter()
            .map(|node| PackageTestCallGraphNode {
                function_index: node.function,
                calls: node.calls,
                forbidden_effects: node
                    .forbidden_effects
                    .into_iter()
                    .map(compiler_forbidden_effect)
                    .collect(),
            })
            .collect();
        let debug_info = PackageDebugInfo {
            root_package_id: "example.tests".into(),
            entry_module: "test.sample".into(),
            modules: Vec::new(),
            functions: debug,
            host_imports: Vec::new(),
        };
        let paths = BTreeMap::from([(FILE, "tests/sample.nexa".into())]);
        (verified, tests, call_graph, debug_info, paths)
    }

    const fn compiler_forbidden_effect(effect: ForbiddenEffect) -> PackageTestForbiddenEffect {
        match effect {
            ForbiddenEffect::Host => PackageTestForbiddenEffect::Host,
            ForbiddenEffect::Task => PackageTestForbiddenEffect::Task,
            ForbiddenEffect::Await => PackageTestForbiddenEffect::Await,
            ForbiddenEffect::Yield => PackageTestForbiddenEffect::Yield,
            ForbiddenEffect::Activation => PackageTestForbiddenEffect::Activation,
            ForbiddenEffect::Migration => PackageTestForbiddenEffect::Migration,
            ForbiddenEffect::PersistentState => PackageTestForbiddenEffect::PersistentState,
        }
    }

    #[test]
    fn pass_and_fail_use_fresh_realms_and_stable_order() {
        let pass_span = CoreSourceSpan::new(FILE, 0, 12);
        let fail_span = CoreSourceSpan::new(FILE, 13, 25);
        let (verified, tests, call_graph, debug, paths) = build_artifact(
            vec![
                ("z_pass".into(), bool_function(true), pass_span),
                ("a_fail".into(), bool_function(false), fail_span),
            ],
            vec![
                test_info(0, "z_pass", pass_span),
                test_info(1, "a_fail", fail_span),
            ],
            |_| {},
        );
        let run = run_package_tests(
            PackageTestArtifactRef {
                verified: &verified,
                tests: &tests,
                call_graph: &call_graph,
                debug_info: &debug,
                source_paths: &paths,
            },
            PackageTestOptions::default(),
        )
        .unwrap();

        assert_eq!(run.summary.passed, 1);
        assert_eq!(run.summary.failed, 1);
        assert_eq!(run.summary.errors, 0);
        assert_eq!(run.results[0].name, "a_fail");
        assert_eq!(run.results[0].status, TestStatus::Fail);
        assert_eq!(run.results[1].name, "z_pass");
        assert_eq!(run.results[1].status, TestStatus::Pass);
        assert!(run.results.iter().all(|result| result.instructions > 0));
    }

    #[test]
    fn expensive_intrinsic_completes_under_one_fixed_total_budget() {
        let span = CoreSourceSpan::new(FILE, 0, 32);
        let (verified, tests, call_graph, debug, paths) = build_artifact(
            vec![("intrinsic".into(), expensive_intrinsic_function(), span)],
            vec![test_info(0, "intrinsic", span)],
            |_| {},
        );
        let run = run_package_tests(
            PackageTestArtifactRef {
                verified: &verified,
                tests: &tests,
                call_graph: &call_graph,
                debug_info: &debug,
                source_paths: &paths,
            },
            PackageTestOptions { fuel_limit: 19 },
        )
        .unwrap();

        assert_eq!(run.results[0].status, TestStatus::Pass);
        assert_eq!(run.results[0].instructions, 4);
        assert_eq!(run.results[0].fuel, 19);
    }

    #[test]
    fn trap_reports_callee_to_caller_source_stack_and_charge() {
        let helper_span = CoreSourceSpan::new(FILE, 2, 8);
        let test_span = CoreSourceSpan::new(FILE, 20, 32);
        let (verified, tests, call_graph, debug, paths) = build_artifact(
            vec![
                ("helper".into(), trap_function(), helper_span),
                ("traps".into(), call_function(0), test_span),
            ],
            vec![test_info(1, "traps", test_span)],
            |_| {},
        );
        let run = run_package_tests(
            PackageTestArtifactRef {
                verified: &verified,
                tests: &tests,
                call_graph: &call_graph,
                debug_info: &debug,
                source_paths: &paths,
            },
            PackageTestOptions::default(),
        )
        .unwrap();
        let result = &run.results[0];

        assert_eq!(result.status, TestStatus::Error);
        assert!(matches!(result.error, Some(TestError::Trap { .. })));
        assert_eq!(
            result
                .stack
                .iter()
                .map(|frame| frame.function.as_str())
                .collect::<Vec<_>>(),
            ["helper", "traps"]
        );
        assert_eq!(
            result.stack[0].span.as_ref().unwrap().source,
            "tests/sample.nexa"
        );
        assert!(result.instructions > 0);
        assert!(result.fuel > 0);
    }

    #[test]
    fn fixed_fuel_limit_reports_exact_charge_and_active_stack() {
        let helper_span = CoreSourceSpan::new(FILE, 0, 40);
        let test_span = CoreSourceSpan::new(FILE, 41, 64);
        let mut helper = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Bool),
            },
            1,
        );
        for _ in 0..32 {
            helper.emit(Instruction::LoadBool {
                dst: 0,
                value: true,
            });
        }
        helper.emit(Instruction::Return { source: 0 });
        let (verified, tests, call_graph, debug, paths) = build_artifact(
            vec![
                ("helper".into(), helper.finish().unwrap(), helper_span),
                ("exhausts".into(), call_function(0), test_span),
            ],
            vec![test_info(1, "exhausts", test_span)],
            |_| {},
        );
        let run = run_package_tests(
            PackageTestArtifactRef {
                verified: &verified,
                tests: &tests,
                call_graph: &call_graph,
                debug_info: &debug,
                source_paths: &paths,
            },
            PackageTestOptions { fuel_limit: 4 },
        )
        .unwrap();

        assert_eq!(run.results[0].status, TestStatus::Error);
        assert_eq!(run.results[0].error, Some(TestError::FuelExhaustion));
        assert_eq!(run.results[0].instructions, 33);
        // Bytecode v6 charges the caller's `Call` base plus its one-register
        // callee-frame initialization (2 fuel) before entering `helper`.
        // The first `LoadBool` is then settled at the callee's entry
        // safepoint, so the exact committed charge is 3 fuel; the remaining
        // straight-line loads stay pending when `Return` exhausts the budget.
        assert_eq!(run.results[0].fuel, 3);
        assert_eq!(
            run.results[0]
                .stack
                .iter()
                .map(|frame| frame.function.as_str())
                .collect::<Vec<_>>(),
            ["helper", "exhausts"]
        );
    }

    #[test]
    fn indirect_host_reachability_is_rejected_before_execution() {
        let helper_span = CoreSourceSpan::new(FILE, 0, 10);
        let test_span = CoreSourceSpan::new(FILE, 11, 24);
        let mut host_helper = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Bool),
            },
            1,
        );
        host_helper
            .emit(Instruction::HostCall {
                import: 0,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let (verified, tests, call_graph, debug, paths) = build_artifact(
            vec![
                (
                    "host_helper".into(),
                    host_helper.finish().unwrap(),
                    helper_span,
                ),
                ("indirect".into(), call_function(0), test_span),
            ],
            vec![test_info(1, "indirect", test_span)],
            |builder| {
                builder.host_import(HostImport {
                    stable_id: StableId::from_name("Host::forbidden"),
                    declaration_fingerprint: [0x74; 32],
                    capabilities: Vec::new(),
                    parameters: Vec::new(),
                    result: Some(ValueType::Bool),
                    mode: HostCallMode::Immediate,
                    fuel_cost: 1,
                    async_result: None,
                });
            },
        );
        let error = run_package_tests(
            PackageTestArtifactRef {
                verified: &verified,
                tests: &tests,
                call_graph: &call_graph,
                debug_info: &debug,
                source_paths: &paths,
            },
            PackageTestOptions::default(),
        )
        .unwrap_err();

        let PackageTestRunError::Ineligible(violations) = error else {
            panic!("expected eligibility rejection");
        };
        assert_eq!(
            violations[0]
                .path
                .iter()
                .map(|location| location.name.as_str())
                .collect::<Vec<_>>(),
            ["indirect", "host_helper"]
        );
        assert_eq!(violations[0].reason, PackageTestEligibilityReason::Host);

        let mut missing_effect = call_graph.clone();
        missing_effect[0].forbidden_effects.clear();
        assert_eq!(
            validate_package_test_artifact(PackageTestArtifactRef {
                verified: &verified,
                tests: &tests,
                call_graph: &missing_effect,
                debug_info: &debug,
                source_paths: &paths,
            }),
            Err(PackageTestRunError::CallGraphMetadataMismatch {
                function: debug
                    .functions
                    .iter()
                    .find(|function| function.function_index == 0)
                    .map(|function| function.stable_id.0),
            })
        );
    }

    #[test]
    fn compiler_only_semantic_effect_is_used_for_eligibility() {
        let span = CoreSourceSpan::new(FILE, 0, 12);
        let (verified, tests, mut call_graph, debug, paths) = build_artifact(
            vec![("valid".into(), bool_function(true), span)],
            vec![test_info(0, "valid", span)],
            |_| {},
        );
        call_graph
            .first_mut()
            .unwrap()
            .forbidden_effects
            .insert(PackageTestForbiddenEffect::Host);

        let error = validate_package_test_artifact(PackageTestArtifactRef {
            verified: &verified,
            tests: &tests,
            call_graph: &call_graph,
            debug_info: &debug,
            source_paths: &paths,
        })
        .unwrap_err();
        let PackageTestRunError::Ineligible(violations) = error else {
            panic!("compiler-only semantic evidence must reject the test");
        };
        assert_eq!(violations[0].reason, PackageTestEligibilityReason::Host);
    }

    #[test]
    fn malformed_root_and_canonical_test_identity_are_rejected() {
        let span = CoreSourceSpan::new(FILE, 0, 12);
        let (verified, mut tests, call_graph, mut debug, paths) = build_artifact(
            vec![("valid".into(), bool_function(true), span)],
            vec![test_info(0, "valid", span)],
            |_| {},
        );

        tests[0].package_id = "dependency.tests".into();
        let error = validate_package_test_artifact(PackageTestArtifactRef {
            verified: &verified,
            tests: &tests,
            call_graph: &call_graph,
            debug_info: &debug,
            source_paths: &paths,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            PackageTestRunError::InvalidDeclarations(ref errors)
                if errors[0].reason == PackageTestDeclarationErrorReason::MetadataMismatch
        ));

        tests[0] = test_info(0, "valid", span);
        let forged = CanonicalSymbolIdentity::automatic(
            "example.tests",
            "test.sample",
            SymbolKind::Function,
            "valid",
        );
        tests[0].stable_id = forged.runtime_id();
        tests[0].canonical_identity = forged.clone();
        debug.functions[0].stable_id = forged.runtime_id();
        debug.functions[0].canonical_identity = forged;
        let error = validate_package_test_artifact(PackageTestArtifactRef {
            verified: &verified,
            tests: &tests,
            call_graph: &call_graph,
            debug_info: &debug,
            source_paths: &paths,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            PackageTestRunError::InvalidDeclarations(ref errors)
                if errors[0].reason == PackageTestDeclarationErrorReason::MetadataMismatch
        ));
    }

    #[test]
    fn every_helper_debug_identity_is_validated_without_rejecting_explicit_stability() {
        let helper_span = CoreSourceSpan::new(FILE, 0, 12);
        let test_span = CoreSourceSpan::new(FILE, 13, 25);
        let (verified, tests, call_graph, mut debug, paths) = build_artifact(
            vec![
                ("helper".into(), bool_function(true), helper_span),
                ("valid".into(), call_function(0), test_span),
            ],
            vec![test_info(1, "valid", test_span)],
            |_| {},
        );

        let explicit =
            CanonicalSymbolIdentity::explicit("example.tests", SymbolKind::Function, "helper-id");
        debug.functions[0].stable_id = explicit.runtime_id();
        debug.functions[0].canonical_identity = explicit;
        validate_package_test_artifact(PackageTestArtifactRef {
            verified: &verified,
            tests: &tests,
            call_graph: &call_graph,
            debug_info: &debug,
            source_paths: &paths,
        })
        .unwrap();

        let malformed = CanonicalSymbolIdentity::automatic(
            "example.tests",
            "wrong.module",
            SymbolKind::Function,
            "helper",
        );
        debug.functions[0].stable_id = malformed.runtime_id();
        debug.functions[0].canonical_identity = malformed;
        let malformed_stable_id = debug.functions[0].stable_id.0;
        assert_eq!(
            validate_package_test_artifact(PackageTestArtifactRef {
                verified: &verified,
                tests: &tests,
                call_graph: &call_graph,
                debug_info: &debug,
                source_paths: &paths,
            }),
            Err(PackageTestRunError::ArtifactMetadataMismatch {
                function: Some(malformed_stable_id),
            })
        );
    }

    #[test]
    fn duplicate_test_diagnostics_do_not_depend_on_metadata_input_order() {
        let first_span = CoreSourceSpan::new(FILE, 0, 12);
        let second_span = CoreSourceSpan::new(FILE, 13, 25);
        let functions = vec![
            ("duplicate".into(), bool_function(true), first_span),
            ("duplicate".into(), bool_function(true), second_span),
        ];
        let forward_tests = vec![
            test_info(0, "duplicate", first_span),
            test_info(1, "duplicate", second_span),
        ];
        let reverse_tests = forward_tests.iter().cloned().rev().collect::<Vec<_>>();
        let forward = build_artifact(functions.clone(), forward_tests, |_| {});
        let reverse = build_artifact(functions, reverse_tests, |_| {});

        let validate = |artifact: &(
            VerifiedModule,
            Vec<PackageTestInfo>,
            Vec<PackageTestCallGraphNode>,
            PackageDebugInfo,
            PackageTestSourcePaths,
        )| {
            validate_package_test_artifact(PackageTestArtifactRef {
                verified: &artifact.0,
                tests: &artifact.1,
                call_graph: &artifact.2,
                debug_info: &artifact.3,
                source_paths: &artifact.4,
            })
            .unwrap_err()
        };
        let forward = validate(&forward);
        let reverse = validate(&reverse);
        assert_eq!(forward, reverse);
        assert!(matches!(
            forward,
            PackageTestRunError::InvalidDeclarations(ref errors)
                if errors.len() == 2
                    && errors.iter().all(|error| {
                        error.reason
                            == PackageTestDeclarationErrorReason::DuplicateQualifiedName
                    })
        ));
    }

    #[test]
    fn missing_host_contract_identity_is_rejected_even_without_tests() {
        let span = CoreSourceSpan::new(FILE, 0, 12);
        let (verified, _tests, call_graph, debug, paths) = build_artifact(
            vec![("helper".into(), bool_function(true), span)],
            Vec::new(),
            |_| {},
        );
        let mut module = verified.module().clone();
        module.host_contract_id = None;
        let verified = verify(module, VerifierLimits::default()).unwrap();

        assert_eq!(
            validate_package_test_artifact(PackageTestArtifactRef {
                verified: &verified,
                tests: &[],
                call_graph: &call_graph,
                debug_info: &debug,
                source_paths: &paths,
            }),
            Err(PackageTestRunError::MissingHostContractRuntimeId)
        );
    }
}
