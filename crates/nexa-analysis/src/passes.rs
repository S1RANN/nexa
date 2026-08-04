//! Typed IR optimization pass manager (M5 WP37) and constant folding
//! (WP38).
//!
//! Passes rewrite one owned `TypedFunctionIr` at a time, immediately before
//! bytecode lowering. Analyzer-owned `TypedPackageIr` snapshots, their
//! fingerprints, and incremental-analysis identity are never mutated.
//!
//! Every rewrite is semantics-preserving under the M5 freeze: folds must
//! reproduce exactly what the interpreter would compute, and any operation
//! that could trap, wrap, allocate, or touch the host at runtime is left
//! untouched. When in doubt a fold is skipped; missing an optimization is
//! recoverable, changing an observable result is not.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::ir::{
    BinaryOperator, BuiltinOperationIr, BuiltinVariantIr, DefinitionId, DefinitionKind, IrEffect,
    IrLiteral, IrType, MigrationIntrinsicIr, TypedBlockIr, TypedDeclarationBody, TypedExpressionIr,
    TypedExpressionKind, TypedFunctionIr, TypedIrError, TypedPackageIr, TypedPatternIr,
    TypedPatternKind, TypedPlaceIr, TypedStatementIr, UnaryOperator, validate_function_for_pass,
};

/// One Typed IR rewrite pass (WP37).
pub trait TypedIrPass {
    fn name(&self) -> &'static str;
    fn run_function(&self, function: &mut TypedFunctionIr, context: &mut PassContext);
}

/// Upper bound for one folded string concatenation. Far below the module
/// decode limits (65,536 strings / 4 MiB total), it keeps repeated folds
/// from inflating the constant table while covering every realistic
/// literal concatenation.
const MAX_FOLDED_STRING_BYTES: usize = 4_096;

/// Mutable bookkeeping shared with a running pass.
#[derive(Debug, Default)]
pub struct PassContext {
    pub rewrites: u64,
    invariant: PassInvariant,
}

impl PassContext {
    fn new(invariant: &PassInvariant) -> Self {
        Self {
            rewrites: 0,
            invariant: invariant.clone(),
        }
    }
}

/// Evidence record for one executed pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassReport {
    pub pass: &'static str,
    pub rewrites: u64,
}

/// Immutable validation context shared by every pass in one package build.
///
/// The definition bound and package constants are captured from an already
/// validated [`TypedPackageIr`]. This lets the pass manager rerun the Typed IR
/// validator before and after every rewrite without reconstructing package
/// graphs or mutating analyzer-owned snapshots.
#[derive(Clone, Debug)]
pub struct PassInvariant {
    definition_limit: usize,
    constants: Arc<BTreeMap<DefinitionId, TypedExpressionIr>>,
    aggregate_definitions: Arc<BTreeSet<DefinitionId>>,
}

impl PassInvariant {
    #[must_use]
    pub fn for_package(package: &TypedPackageIr) -> Self {
        let constants = package
            .modules()
            .iter()
            .flat_map(|module| module.declarations.iter())
            .filter_map(|declaration| match &declaration.body {
                TypedDeclarationBody::Const(expression) => {
                    Some((declaration.definition, expression.clone()))
                }
                TypedDeclarationBody::Function(_)
                | TypedDeclarationBody::TypeLayout(_)
                | TypedDeclarationBody::External => None,
            })
            .collect();
        let aggregate_definitions = package
            .definitions()
            .iter()
            .filter(|definition| {
                matches!(
                    definition.kind,
                    DefinitionKind::Struct | DefinitionKind::Enum
                )
            })
            .map(|definition| definition.id)
            .collect();
        Self {
            definition_limit: package.definitions().len(),
            constants: Arc::new(constants),
            aggregate_definitions: Arc::new(aggregate_definitions),
        }
    }

    /// A structural validation context for isolated pass unit tests.
    ///
    /// Production compilation always uses [`Self::for_package`]. The
    /// effectively unbounded definition space here permits fixtures to use
    /// compact synthetic IDs without manufacturing a full package.
    #[must_use]
    pub fn isolated() -> Self {
        Self {
            definition_limit: usize::MAX,
            constants: Arc::default(),
            aggregate_definitions: Arc::default(),
        }
    }

    pub fn validate(&self, function: &TypedFunctionIr) -> Result<(), TypedIrError> {
        validate_function_for_pass(function, self.definition_limit, &self.constants)
    }

    fn constant(&self, definition: DefinitionId) -> Option<&TypedExpressionIr> {
        self.constants.get(&definition)
    }

    fn is_aggregate_type(&self, ty: &IrType) -> bool {
        match ty {
            IrType::Named(definition) => self.aggregate_definitions.contains(definition),
            IrType::Option(_) | IrType::Result(_, _) | IrType::Tuple(_) => true,
            IrType::Unit
            | IrType::Bool
            | IrType::I32
            | IrType::I64
            | IrType::F32
            | IrType::F64
            | IrType::String
            | IrType::Rune
            | IrType::Array(_)
            | IrType::Map(_, _)
            | IrType::HostRequest(_)
            | IrType::ResourceToken(_)
            | IrType::Snapshot(_)
            | IrType::Buffer(_)
            | IrType::StateHandle(_)
            | IrType::TypeParameter(_) => false,
        }
    }
}

impl Default for PassInvariant {
    fn default() -> Self {
        Self::isolated()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassInvariantPhase {
    Before,
    After,
}

/// A pass bug is surfaced as a compiler error instead of allowing malformed IR
/// to reach bytecode lowering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassInvariantViolation {
    pub pass: &'static str,
    pub phase: PassInvariantPhase,
    pub error: TypedIrError,
}

impl fmt::Display for PassInvariantViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Typed IR invariant failed {:?} pass `{}`: {}",
            self.phase, self.pass, self.error
        )
    }
}

impl std::error::Error for PassInvariantViolation {}

/// Ordered pass pipeline over owned function IR.
pub struct PassManager {
    passes: Vec<Box<dyn TypedIrPass>>,
}

impl PassManager {
    /// The standard M5 pipeline; grows as stage-D passes land.
    ///
    /// Folding runs again after propagation because substituted literals
    /// expose new folds; the pipeline is a fixed bounded sequence, never an
    /// unbounded fixpoint iteration.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            passes: vec![
                Box::new(ConstantFolding),
                Box::new(ConstantPropagation),
                Box::new(ConstantFolding),
                Box::new(CopyPropagation),
                Box::new(MatchSpecialization),
                Box::new(ConstantFolding),
                Box::new(DeadCodeElimination),
            ],
        }
    }

    /// Runs every pass over one function, returning per-pass evidence.
    pub fn optimize_function(&self, function: &mut TypedFunctionIr) -> Vec<PassReport> {
        self.optimize_function_checked(function, &PassInvariant::isolated())
            .expect("isolated Typed IR pass fixture must preserve invariants")
    }

    /// Runs every pass with package-aware validation before and after each
    /// rewrite.
    pub fn optimize_function_checked(
        &self,
        function: &mut TypedFunctionIr,
        invariant: &PassInvariant,
    ) -> Result<Vec<PassReport>, PassInvariantViolation> {
        let mut reports = Vec::with_capacity(self.passes.len());
        for pass in &self.passes {
            invariant
                .validate(function)
                .map_err(|error| PassInvariantViolation {
                    pass: pass.name(),
                    phase: PassInvariantPhase::Before,
                    error,
                })?;
            let mut context = PassContext::new(invariant);
            pass.run_function(function, &mut context);
            invariant
                .validate(function)
                .map_err(|error| PassInvariantViolation {
                    pass: pass.name(),
                    phase: PassInvariantPhase::After,
                    error,
                })?;
            reports.push(PassReport {
                pass: pass.name(),
                rewrites: context.rewrites,
            });
        }
        Ok(reports)
    }

    /// Runs the complete package-aware Stage-D pipeline.
    ///
    /// Local rewrites run first, then WP44 performs one bounded cross-function
    /// inlining wave, and local cleanup runs once more on callers changed by
    /// inlining. The final WP45 report records every aggregate boundary seen
    /// by lowering.
    pub fn optimize_functions(
        &self,
        functions: &mut BTreeMap<DefinitionId, TypedFunctionIr>,
        invariant: &PassInvariant,
    ) -> Result<PackageOptimizationReport, PassInvariantViolation> {
        let mut function_reports = BTreeMap::new();
        for (definition, function) in functions.iter_mut() {
            function_reports.insert(
                *definition,
                self.optimize_function_checked(function, invariant)?,
            );
        }

        for function in functions.values() {
            invariant
                .validate(function)
                .map_err(|error| PassInvariantViolation {
                    pass: "small-function-inlining",
                    phase: PassInvariantPhase::Before,
                    error,
                })?;
        }
        let (inlining_rewrites, changed_callers) = inline_small_functions(functions);
        for function in functions.values() {
            invariant
                .validate(function)
                .map_err(|error| PassInvariantViolation {
                    pass: "small-function-inlining",
                    phase: PassInvariantPhase::After,
                    error,
                })?;
        }

        for caller in changed_callers {
            let function = functions
                .get_mut(&caller)
                .expect("changed inlining caller remains in the package");
            function_reports
                .entry(caller)
                .or_default()
                .extend(self.optimize_function_checked(function, invariant)?);
        }

        Ok(PackageOptimizationReport {
            function_reports,
            inlining: PassReport {
                pass: "small-function-inlining",
                rewrites: inlining_rewrites,
            },
            materialization: analyze_aggregate_materialization(functions, invariant),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageOptimizationReport {
    pub function_reports: BTreeMap<DefinitionId, Vec<PassReport>>,
    pub inlining: PassReport,
    pub materialization: AggregateMaterializationReport,
}

/// WP45 evidence: aggregate values remain physical inside ordinary local
/// computation and cross an explicit boundary only at one of these sites.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AggregateMaterializationReport {
    pub local_physical_values: u64,
    pub container_boundaries: u64,
    pub task_suspend_boundaries: u64,
    pub host_boundaries: u64,
    pub persistent_state_boundaries: u64,
}

impl AggregateMaterializationReport {
    #[must_use]
    pub const fn total_boundaries(&self) -> u64 {
        self.container_boundaries
            .saturating_add(self.task_suspend_boundaries)
            .saturating_add(self.host_boundaries)
            .saturating_add(self.persistent_state_boundaries)
    }
}

const INLINE_CALL_THRESHOLD: u64 = 2;
const MAX_INLINE_BODY_NODES: usize = 12;
const MAX_INLINE_GROWTH_PER_CALLER: usize = 64;

#[derive(Clone, Debug)]
struct InlineCandidate {
    parameters: Vec<DefinitionId>,
    body: TypedExpressionIr,
}

fn inline_small_functions(
    functions: &mut BTreeMap<DefinitionId, TypedFunctionIr>,
) -> (u64, BTreeSet<DefinitionId>) {
    let call_counts = package_call_counts(functions);
    let candidates = functions
        .iter()
        .filter_map(|(definition, function)| {
            let calls = call_counts.get(definition).copied().unwrap_or_default();
            (calls >= INLINE_CALL_THRESHOLD)
                .then(|| inline_candidate(function))
                .flatten()
                .map(|candidate| (*definition, candidate))
        })
        .collect::<BTreeMap<_, _>>();

    let mut rewrites = 0_u64;
    let mut changed = BTreeSet::new();
    for (caller, function) in functions {
        let mut growth = 0_usize;
        let before = rewrites;
        inline_calls_in_block(
            &mut function.body,
            *caller,
            &candidates,
            &mut growth,
            &mut rewrites,
        );
        if rewrites != before {
            changed.insert(*caller);
        }
    }
    (rewrites, changed)
}

fn inline_candidate(function: &TypedFunctionIr) -> Option<InlineCandidate> {
    if !matches!(function.effect, IrEffect::Ordinary | IrEffect::Immediate)
        || !function.locals.is_empty()
        || !is_inline_result_type(&function.return_type)
    {
        return None;
    }
    let body = match (&function.body.statements[..], function.body.tail.as_deref()) {
        ([], Some(tail)) => tail,
        ([TypedStatementIr::Return(Some(value))], None) => value,
        _ => return None,
    };
    if body.ty != function.return_type
        || expression_node_count(body) > MAX_INLINE_BODY_NODES
        || !inline_body_is_safe(body, &function.parameters.iter().copied().collect())
    {
        return None;
    }
    Some(InlineCandidate {
        parameters: function.parameters.clone(),
        body: body.clone(),
    })
}

const fn is_inline_result_type(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::Bool | IrType::I32 | IrType::I64 | IrType::F32 | IrType::F64 | IrType::Rune
    )
}

fn inline_body_is_safe(
    expression: &TypedExpressionIr,
    parameters: &BTreeSet<DefinitionId>,
) -> bool {
    match &expression.kind {
        TypedExpressionKind::Literal(literal) => {
            !matches!(literal, IrLiteral::String(_) | IrLiteral::Unit)
        }
        TypedExpressionKind::Reference(definition) => parameters.contains(definition),
        TypedExpressionKind::Unary { operand, .. } => {
            expression.ty != IrType::String && inline_body_is_safe(operand, parameters)
        }
        TypedExpressionKind::Binary {
            operator,
            left,
            right,
        } => {
            !matches!(operator, BinaryOperator::Divide | BinaryOperator::Remainder)
                && expression.ty != IrType::String
                && left.ty != IrType::String
                && right.ty != IrType::String
                && inline_body_is_safe(left, parameters)
                && inline_body_is_safe(right, parameters)
        }
        _ => false,
    }
}

fn inline_argument_is_safe(expression: &TypedExpressionIr) -> bool {
    match &expression.kind {
        TypedExpressionKind::Literal(literal) => !matches!(literal, IrLiteral::String(_)),
        TypedExpressionKind::Reference(_) => true,
        TypedExpressionKind::Unary { operand, .. } => {
            expression.ty != IrType::String && inline_argument_is_safe(operand)
        }
        TypedExpressionKind::Binary {
            operator,
            left,
            right,
        } => {
            !matches!(operator, BinaryOperator::Divide | BinaryOperator::Remainder)
                && expression.ty != IrType::String
                && left.ty != IrType::String
                && right.ty != IrType::String
                && inline_argument_is_safe(left)
                && inline_argument_is_safe(right)
        }
        _ => false,
    }
}

fn expression_node_count(expression: &TypedExpressionIr) -> usize {
    let mut children = Vec::new();
    collect_children(expression, &mut children);
    children.into_iter().fold(1_usize, |count, child| {
        count.saturating_add(expression_node_count(child))
    })
}

fn package_call_counts(
    functions: &BTreeMap<DefinitionId, TypedFunctionIr>,
) -> BTreeMap<DefinitionId, u64> {
    let mut counts = BTreeMap::new();
    for function in functions.values() {
        count_calls_in_block(&function.body, &mut counts);
    }
    counts
}

fn count_calls_in_block(block: &TypedBlockIr, counts: &mut BTreeMap<DefinitionId, u64>) {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    count_calls_in_expression(value, counts);
                }
            }
            TypedStatementIr::Assign { target, value } => {
                count_calls_in_place(target, counts);
                count_calls_in_expression(value, counts);
            }
            TypedStatementIr::Expression(expression) => {
                count_calls_in_expression(expression, counts);
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                count_calls_in_expression(condition, counts);
                count_calls_in_block(then_block, counts);
                if let Some(else_block) = else_block {
                    count_calls_in_block(else_block, counts);
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                count_calls_in_expression(condition, counts);
                count_calls_in_block(body, counts);
            }
            TypedStatementIr::StaticRangeFor {
                start, end, body, ..
            } => {
                count_calls_in_expression(start, counts);
                count_calls_in_expression(end, counts);
                count_calls_in_block(body, counts);
            }
            TypedStatementIr::Defer { captures, .. } => {
                for capture in captures {
                    count_calls_in_expression(capture, counts);
                }
            }
            TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    if let Some(tail) = &block.tail {
        count_calls_in_expression(tail, counts);
    }
}

fn count_calls_in_place(place: &TypedPlaceIr, counts: &mut BTreeMap<DefinitionId, u64>) {
    match place {
        TypedPlaceIr::Definition(_) => {}
        TypedPlaceIr::Field { base, .. } => count_calls_in_place(base, counts),
        TypedPlaceIr::ClassField { object, .. } | TypedPlaceIr::StateField { base: object, .. } => {
            count_calls_in_expression(object, counts);
        }
        TypedPlaceIr::Index { base, index } => {
            count_calls_in_expression(base, counts);
            count_calls_in_expression(index, counts);
        }
    }
}

fn count_calls_in_expression(
    expression: &TypedExpressionIr,
    counts: &mut BTreeMap<DefinitionId, u64>,
) {
    if let TypedExpressionKind::Call { callee, .. } = expression.kind {
        let count = counts.entry(callee).or_default();
        *count = count.saturating_add(1);
    }
    let mut children = Vec::new();
    collect_children(expression, &mut children);
    for child in children {
        count_calls_in_expression(child, counts);
    }
}

fn inline_calls_in_block(
    block: &mut TypedBlockIr,
    caller: DefinitionId,
    candidates: &BTreeMap<DefinitionId, InlineCandidate>,
    growth: &mut usize,
    rewrites: &mut u64,
) {
    for statement in &mut block.statements {
        match statement {
            TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    inline_calls_in_expression(value, caller, candidates, growth, rewrites);
                }
            }
            TypedStatementIr::Assign { target, value } => {
                inline_calls_in_place(target, caller, candidates, growth, rewrites);
                inline_calls_in_expression(value, caller, candidates, growth, rewrites);
            }
            TypedStatementIr::Expression(expression) => {
                inline_calls_in_expression(expression, caller, candidates, growth, rewrites);
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                inline_calls_in_expression(condition, caller, candidates, growth, rewrites);
                inline_calls_in_block(then_block, caller, candidates, growth, rewrites);
                if let Some(else_block) = else_block {
                    inline_calls_in_block(else_block, caller, candidates, growth, rewrites);
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                inline_calls_in_expression(condition, caller, candidates, growth, rewrites);
                inline_calls_in_block(body, caller, candidates, growth, rewrites);
            }
            TypedStatementIr::StaticRangeFor {
                start, end, body, ..
            } => {
                inline_calls_in_expression(start, caller, candidates, growth, rewrites);
                inline_calls_in_expression(end, caller, candidates, growth, rewrites);
                inline_calls_in_block(body, caller, candidates, growth, rewrites);
            }
            TypedStatementIr::Defer { captures, .. } => {
                for capture in captures {
                    inline_calls_in_expression(capture, caller, candidates, growth, rewrites);
                }
            }
            TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        inline_calls_in_expression(tail, caller, candidates, growth, rewrites);
    }
}

fn inline_calls_in_place(
    place: &mut TypedPlaceIr,
    caller: DefinitionId,
    candidates: &BTreeMap<DefinitionId, InlineCandidate>,
    growth: &mut usize,
    rewrites: &mut u64,
) {
    match place {
        TypedPlaceIr::Definition(_) => {}
        TypedPlaceIr::Field { base, .. } => {
            inline_calls_in_place(base, caller, candidates, growth, rewrites);
        }
        TypedPlaceIr::ClassField { object, .. } | TypedPlaceIr::StateField { base: object, .. } => {
            inline_calls_in_expression(object, caller, candidates, growth, rewrites);
        }
        TypedPlaceIr::Index { base, index } => {
            inline_calls_in_expression(base, caller, candidates, growth, rewrites);
            inline_calls_in_expression(index, caller, candidates, growth, rewrites);
        }
    }
}

fn inline_calls_in_expression(
    expression: &mut TypedExpressionIr,
    caller: DefinitionId,
    candidates: &BTreeMap<DefinitionId, InlineCandidate>,
    growth: &mut usize,
    rewrites: &mut u64,
) {
    for_each_child_expression(expression, &mut |child| {
        inline_calls_in_expression(child, caller, candidates, growth, rewrites);
    });
    let TypedExpressionKind::Call { callee, arguments } = &expression.kind else {
        return;
    };
    if *callee == caller || !arguments.iter().all(inline_argument_is_safe) {
        return;
    }
    let Some(candidate) = candidates.get(callee) else {
        return;
    };
    if candidate.parameters.len() != arguments.len() {
        return;
    }
    let substitutions = candidate
        .parameters
        .iter()
        .copied()
        .zip(arguments.iter().cloned())
        .collect::<BTreeMap<_, _>>();
    let mut replacement = candidate.body.clone();
    substitute_inline_parameters(&mut replacement, &substitutions);
    let added_nodes = expression_node_count(&replacement).saturating_sub(1);
    if growth.saturating_add(added_nodes) > MAX_INLINE_GROWTH_PER_CALLER {
        return;
    }
    *growth = growth.saturating_add(added_nodes);
    replacement.span = expression.span.clone();
    *expression = replacement;
    *rewrites = rewrites.saturating_add(1);
}

fn substitute_inline_parameters(
    expression: &mut TypedExpressionIr,
    substitutions: &BTreeMap<DefinitionId, TypedExpressionIr>,
) {
    if let TypedExpressionKind::Reference(definition) = expression.kind
        && let Some(argument) = substitutions.get(&definition)
    {
        *expression = argument.clone();
        return;
    }
    for_each_child_expression(expression, &mut |child| {
        substitute_inline_parameters(child, substitutions);
    });
}

fn analyze_aggregate_materialization(
    functions: &BTreeMap<DefinitionId, TypedFunctionIr>,
    invariant: &PassInvariant,
) -> AggregateMaterializationReport {
    let mut report = AggregateMaterializationReport::default();
    for function in functions.values() {
        analyze_materialization_block(&function.body, invariant, &mut report);
    }
    report
}

fn analyze_materialization_block(
    block: &TypedBlockIr,
    invariant: &PassInvariant,
    report: &mut AggregateMaterializationReport,
) {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    analyze_materialization_expression(value, invariant, report);
                }
            }
            TypedStatementIr::Assign { target, value } => {
                if invariant.is_aggregate_type(&value.ty) {
                    match target {
                        TypedPlaceIr::Index { .. } => {
                            report.container_boundaries =
                                report.container_boundaries.saturating_add(1);
                        }
                        TypedPlaceIr::StateField { .. } => {
                            report.persistent_state_boundaries =
                                report.persistent_state_boundaries.saturating_add(1);
                        }
                        TypedPlaceIr::Definition(_)
                        | TypedPlaceIr::Field { .. }
                        | TypedPlaceIr::ClassField { .. } => {}
                    }
                }
                analyze_materialization_place(target, invariant, report);
                analyze_materialization_expression(value, invariant, report);
            }
            TypedStatementIr::Expression(expression) => {
                analyze_materialization_expression(expression, invariant, report);
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                analyze_materialization_expression(condition, invariant, report);
                analyze_materialization_block(then_block, invariant, report);
                if let Some(else_block) = else_block {
                    analyze_materialization_block(else_block, invariant, report);
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                analyze_materialization_expression(condition, invariant, report);
                analyze_materialization_block(body, invariant, report);
            }
            TypedStatementIr::StaticRangeFor {
                start, end, body, ..
            } => {
                analyze_materialization_expression(start, invariant, report);
                analyze_materialization_expression(end, invariant, report);
                analyze_materialization_block(body, invariant, report);
            }
            TypedStatementIr::Defer { captures, .. } => {
                for capture in captures {
                    analyze_materialization_expression(capture, invariant, report);
                }
            }
            TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    if let Some(tail) = &block.tail {
        analyze_materialization_expression(tail, invariant, report);
    }
}

fn analyze_materialization_place(
    place: &TypedPlaceIr,
    invariant: &PassInvariant,
    report: &mut AggregateMaterializationReport,
) {
    match place {
        TypedPlaceIr::Definition(_) => {}
        TypedPlaceIr::Field { base, .. } => {
            analyze_materialization_place(base, invariant, report);
        }
        TypedPlaceIr::ClassField { object, .. } | TypedPlaceIr::StateField { base: object, .. } => {
            analyze_materialization_expression(object, invariant, report);
        }
        TypedPlaceIr::Index { base, index } => {
            analyze_materialization_expression(base, invariant, report);
            analyze_materialization_expression(index, invariant, report);
        }
    }
}

fn analyze_materialization_expression(
    expression: &TypedExpressionIr,
    invariant: &PassInvariant,
    report: &mut AggregateMaterializationReport,
) {
    let aggregate = invariant.is_aggregate_type(&expression.ty);
    match &expression.kind {
        TypedExpressionKind::Construct { .. }
        | TypedExpressionKind::EnumConstruct { .. }
        | TypedExpressionKind::BuiltinVariant { .. }
        | TypedExpressionKind::Tuple(_)
            if aggregate =>
        {
            report.local_physical_values = report.local_physical_values.saturating_add(1);
        }
        TypedExpressionKind::BuiltinCall {
            operation,
            arguments,
            ..
        } if is_collection_boundary(*operation) => {
            let values = u64::try_from(
                arguments
                    .iter()
                    .filter(|argument| invariant.is_aggregate_type(&argument.ty))
                    .count(),
            )
            .unwrap_or(u64::MAX)
            .saturating_add(u64::from(aggregate));
            report.container_boundaries = report.container_boundaries.saturating_add(values);
        }
        TypedExpressionKind::Index { .. } if aggregate => {
            report.container_boundaries = report.container_boundaries.saturating_add(1);
        }
        TypedExpressionKind::HostCall { arguments, .. } => {
            let values = u64::try_from(
                arguments
                    .iter()
                    .filter(|argument| invariant.is_aggregate_type(&argument.ty))
                    .count(),
            )
            .unwrap_or(u64::MAX)
            .saturating_add(u64::from(aggregate));
            report.host_boundaries = report.host_boundaries.saturating_add(values);
        }
        TypedExpressionKind::Await(inner) => {
            let values = u64::from(aggregate) + u64::from(invariant.is_aggregate_type(&inner.ty));
            report.task_suspend_boundaries = report.task_suspend_boundaries.saturating_add(values);
        }
        TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::StateField { .. }
        | TypedExpressionKind::Migration(_)
            if aggregate =>
        {
            report.persistent_state_boundaries =
                report.persistent_state_boundaries.saturating_add(1);
        }
        _ => {}
    }

    let mut children = Vec::new();
    collect_children(expression, &mut children);
    for child in children {
        analyze_materialization_expression(child, invariant, report);
    }
}

const fn is_collection_boundary(operation: BuiltinOperationIr) -> bool {
    matches!(
        operation,
        BuiltinOperationIr::ArrayGet
            | BuiltinOperationIr::ArraySet
            | BuiltinOperationIr::ArrayPush
            | BuiltinOperationIr::ArrayPop
            | BuiltinOperationIr::ArrayInsert
            | BuiltinOperationIr::ArrayRemove
            | BuiltinOperationIr::MapGet
            | BuiltinOperationIr::MapSet
            | BuiltinOperationIr::MapRemove
    )
}

/// WP38: folds literal-only scalar expressions while preserving every trap,
/// overflow, allocation, and float-division behavior for the runtime.
pub struct ConstantFolding;

impl TypedIrPass for ConstantFolding {
    fn name(&self) -> &'static str {
        "constant-folding"
    }

    fn run_function(&self, function: &mut TypedFunctionIr, context: &mut PassContext) {
        fold_block(&mut function.body, context);
    }
}

/// WP39: substitutes references to immutable scalar- and string-literal
/// bindings.
///
/// String literals propagate since the WP56 constant pool landed: every
/// `LoadString` of the same content shares one interned heap entry, so
/// duplicating a literal into its use sites no longer multiplies runtime
/// allocations. The binding itself stays in place for WP41 to remove once
/// it is provably unused.
pub struct ConstantPropagation;

impl TypedIrPass for ConstantPropagation {
    fn name(&self) -> &'static str {
        "constant-propagation"
    }

    fn run_function(&self, function: &mut TypedFunctionIr, context: &mut PassContext) {
        let mut scopes = PropagationScopes::new(function);
        propagate_block(
            &mut function.body,
            &mut scopes,
            PropagationMode::Constants,
            context,
        );
    }
}

/// WP40: rewrites `let alias = source;` chains so later references use the
/// original immutable binding directly.
pub struct CopyPropagation;

impl TypedIrPass for CopyPropagation {
    fn name(&self) -> &'static str {
        "copy-propagation"
    }

    fn run_function(&self, function: &mut TypedFunctionIr, context: &mut PassContext) {
        let mut scopes = PropagationScopes::new(function);
        propagate_block(
            &mut function.body,
            &mut scopes,
            PropagationMode::Copies,
            context,
        );
    }
}

/// WP43: removes match dispatch when the scrutinee's literal or enum tag is
/// already known locally.
///
/// Analysis has completed exhaustiveness diagnostics before this pass runs.
/// Specialization therefore only selects an already-validated arm. Direct
/// constructors are specialized only when evaluating every payload is
/// non-trapping; locally stored constructors may always contribute their tag
/// because their initializer has already executed.
pub struct MatchSpecialization;

impl TypedIrPass for MatchSpecialization {
    fn name(&self) -> &'static str {
        "match-specialization"
    }

    fn run_function(&self, function: &mut TypedFunctionIr, context: &mut PassContext) {
        let mut scopes = MatchScopes::default();
        specialize_match_block(&mut function.body, &mut scopes, context);
    }
}

#[derive(Clone, Debug)]
enum KnownTag {
    Literal(IrLiteral),
    Enum(DefinitionId),
    Builtin(BuiltinVariantIr),
    Struct(DefinitionId),
}

#[derive(Default)]
struct MatchScopes {
    scopes: Vec<BTreeMap<DefinitionId, KnownTag>>,
}

impl MatchScopes {
    fn lookup(&self, definition: DefinitionId) -> Option<&KnownTag> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&definition))
    }

    fn bind(&mut self, definition: DefinitionId, tag: KnownTag) {
        self.scopes
            .last_mut()
            .expect("match specialization always keeps one scope")
            .insert(definition, tag);
    }

    fn invalidate(&mut self, definition: DefinitionId) {
        for scope in &mut self.scopes {
            scope.remove(&definition);
        }
    }
}

#[derive(Clone, Debug)]
enum KnownShape {
    Literal(IrLiteral),
    Enum {
        variant: DefinitionId,
        payload: Option<Box<TypedExpressionIr>>,
        projectable: bool,
    },
    Builtin {
        variant: BuiltinVariantIr,
        payload: Option<Box<TypedExpressionIr>>,
        projectable: bool,
    },
    Struct {
        definition: DefinitionId,
        fields: Option<Vec<(DefinitionId, TypedExpressionIr)>>,
    },
}

#[derive(Clone, Debug)]
struct KnownMatchValue {
    whole: TypedExpressionIr,
    shape: KnownShape,
}

fn specialize_match_block(
    block: &mut TypedBlockIr,
    scopes: &mut MatchScopes,
    context: &mut PassContext,
) {
    scopes.scopes.push(BTreeMap::new());
    for statement in &mut block.statements {
        match statement {
            TypedStatementIr::Let {
                definition,
                mutable,
                value,
            } => {
                if let Some(value) = value {
                    specialize_match_expression(value, scopes, context);
                }
                if !*mutable
                    && let Some(tag) = value.as_ref().and_then(|value| known_tag(value, scopes))
                {
                    scopes.bind(*definition, tag);
                }
            }
            TypedStatementIr::Assign { target, value } => {
                specialize_match_place(target, scopes, context);
                specialize_match_expression(value, scopes, context);
                if let Some(definition) = place_root_definition(target) {
                    scopes.invalidate(definition);
                }
            }
            TypedStatementIr::Expression(expression) => {
                specialize_match_expression(expression, scopes, context);
            }
            TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    specialize_match_expression(value, scopes, context);
                }
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                specialize_match_expression(condition, scopes, context);
                specialize_match_block(then_block, scopes, context);
                if let Some(else_block) = else_block {
                    specialize_match_block(else_block, scopes, context);
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                specialize_match_expression(condition, scopes, context);
                specialize_match_block(body, scopes, context);
            }
            TypedStatementIr::StaticRangeFor {
                start, end, body, ..
            } => {
                specialize_match_expression(start, scopes, context);
                specialize_match_expression(end, scopes, context);
                specialize_match_block(body, scopes, context);
            }
            TypedStatementIr::Defer { captures, .. } => {
                for capture in captures {
                    specialize_match_expression(capture, scopes, context);
                }
            }
            TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        specialize_match_expression(tail, scopes, context);
    }
    scopes.scopes.pop();
}

fn specialize_match_place(
    place: &mut TypedPlaceIr,
    scopes: &mut MatchScopes,
    context: &mut PassContext,
) {
    match place {
        TypedPlaceIr::Definition(_) => {}
        TypedPlaceIr::Field { base, .. } => specialize_match_place(base, scopes, context),
        TypedPlaceIr::ClassField { object, .. } | TypedPlaceIr::StateField { base: object, .. } => {
            specialize_match_expression(object, scopes, context);
        }
        TypedPlaceIr::Index { base, index } => {
            specialize_match_expression(base, scopes, context);
            specialize_match_expression(index, scopes, context);
        }
    }
}

fn specialize_match_expression(
    expression: &mut TypedExpressionIr,
    scopes: &MatchScopes,
    context: &mut PassContext,
) {
    for_each_child_expression(expression, &mut |child| {
        specialize_match_expression(child, scopes, context);
    });
    let Some(replacement) = specialized_match(expression, scopes, context) else {
        return;
    };
    *expression = replacement;
    context.rewrites = context.rewrites.saturating_add(1);
}

fn known_tag(expression: &TypedExpressionIr, scopes: &MatchScopes) -> Option<KnownTag> {
    match &expression.kind {
        TypedExpressionKind::Literal(literal) => Some(KnownTag::Literal(literal.clone())),
        TypedExpressionKind::EnumConstruct {
            variant_definition, ..
        } => Some(KnownTag::Enum(*variant_definition)),
        TypedExpressionKind::BuiltinVariant { variant, .. } => Some(KnownTag::Builtin(*variant)),
        TypedExpressionKind::Construct { definition, .. } => Some(KnownTag::Struct(*definition)),
        TypedExpressionKind::Reference(definition) => scopes.lookup(*definition).cloned(),
        _ => None,
    }
}

fn known_match_value(
    expression: &TypedExpressionIr,
    scopes: &MatchScopes,
) -> Option<KnownMatchValue> {
    let whole = expression.clone();
    let shape = match &expression.kind {
        TypedExpressionKind::Literal(literal) => KnownShape::Literal(literal.clone()),
        TypedExpressionKind::EnumConstruct {
            variant_definition,
            payload,
            ..
        } if payload.as_deref().is_none_or(is_match_specialization_safe) => KnownShape::Enum {
            variant: *variant_definition,
            payload: payload.clone(),
            projectable: true,
        },
        TypedExpressionKind::BuiltinVariant { variant, payload }
            if payload.as_deref().is_none_or(is_match_specialization_safe) =>
        {
            KnownShape::Builtin {
                variant: *variant,
                payload: payload.clone(),
                projectable: true,
            }
        }
        TypedExpressionKind::Construct { definition, fields }
            if fields
                .iter()
                .all(|(_, value)| is_match_specialization_safe(value)) =>
        {
            KnownShape::Struct {
                definition: *definition,
                fields: Some(fields.clone()),
            }
        }
        TypedExpressionKind::Reference(definition) => match scopes.lookup(*definition)? {
            KnownTag::Literal(literal) => KnownShape::Literal(literal.clone()),
            KnownTag::Enum(variant) => KnownShape::Enum {
                variant: *variant,
                payload: None,
                projectable: false,
            },
            KnownTag::Builtin(variant) => KnownShape::Builtin {
                variant: *variant,
                payload: None,
                projectable: false,
            },
            KnownTag::Struct(definition) => KnownShape::Struct {
                definition: *definition,
                fields: None,
            },
        },
        _ => return None,
    };
    Some(KnownMatchValue { whole, shape })
}

fn specialized_match(
    expression: &TypedExpressionIr,
    scopes: &MatchScopes,
    context: &mut PassContext,
) -> Option<TypedExpressionIr> {
    let TypedExpressionKind::Match { value, arms } = &expression.kind else {
        return None;
    };
    let known = known_match_value(value, scopes)?;
    for arm in arms {
        if !pattern_matches_shape(&arm.pattern, &known.shape) {
            continue;
        }
        let mut replacements = BTreeMap::new();
        if !bind_known_pattern(&arm.pattern, &known, &arm.value, &mut replacements) {
            continue;
        }
        let mut replacement = arm.value.clone();
        replace_pattern_bindings(&mut replacement, &replacements, context);
        return Some(replacement);
    }
    None
}

fn pattern_matches_shape(pattern: &TypedPatternIr, shape: &KnownShape) -> bool {
    match (&pattern.kind, shape) {
        (TypedPatternKind::Wildcard | TypedPatternKind::Binding(_), _) => true,
        (TypedPatternKind::Literal(expected), KnownShape::Literal(actual)) => expected == actual,
        (TypedPatternKind::Variant { definition, .. }, KnownShape::Enum { variant, .. }) => {
            definition == variant
        }
        (
            TypedPatternKind::BuiltinVariant { variant, .. },
            KnownShape::Builtin {
                variant: actual, ..
            },
        ) => variant == actual,
        (
            TypedPatternKind::Struct { definition, .. },
            KnownShape::Struct {
                definition: actual, ..
            },
        ) => definition == actual,
        _ => false,
    }
}

fn bind_known_pattern(
    pattern: &TypedPatternIr,
    known: &KnownMatchValue,
    arm_value: &TypedExpressionIr,
    replacements: &mut BTreeMap<DefinitionId, TypedExpressionIr>,
) -> bool {
    match (&pattern.kind, &known.shape) {
        (TypedPatternKind::Wildcard | TypedPatternKind::Literal(_), _) => true,
        (TypedPatternKind::Binding(definition), _) => {
            replacements.insert(*definition, known.whole.clone());
            true
        }
        (
            TypedPatternKind::Variant { payload, .. },
            KnownShape::Enum {
                payload: value,
                projectable,
                ..
            },
        ) => bind_variant_payload(
            payload,
            value.as_deref(),
            *projectable,
            arm_value,
            replacements,
        ),
        (
            TypedPatternKind::BuiltinVariant { payload, .. },
            KnownShape::Builtin {
                payload: value,
                projectable,
                ..
            },
        ) => match payload.as_deref() {
            None => true,
            Some(pattern) => bind_one_projected_pattern(
                pattern,
                value.as_deref(),
                *projectable,
                arm_value,
                replacements,
            ),
        },
        (
            TypedPatternKind::Struct { fields, .. },
            KnownShape::Struct {
                fields: Some(values),
                ..
            },
        ) => fields.iter().all(|(field, pattern)| {
            values
                .iter()
                .find(|(candidate, _)| candidate == field)
                .is_some_and(|(_, value)| {
                    bind_direct_pattern(pattern, value, arm_value, replacements)
                })
        }),
        (TypedPatternKind::Struct { fields, .. }, KnownShape::Struct { fields: None, .. }) => {
            fields
                .iter()
                .all(|(_, pattern)| unavailable_pattern_is_safe(pattern, arm_value))
        }
        _ => false,
    }
}

fn bind_variant_payload(
    patterns: &[TypedPatternIr],
    payload: Option<&TypedExpressionIr>,
    projectable: bool,
    arm_value: &TypedExpressionIr,
    replacements: &mut BTreeMap<DefinitionId, TypedExpressionIr>,
) -> bool {
    if patterns.is_empty() {
        return true;
    }
    if !projectable {
        return patterns
            .iter()
            .all(|pattern| unavailable_pattern_is_safe(pattern, arm_value));
    }
    let Some(payload) = payload else {
        return false;
    };
    if patterns.len() == 1 {
        return bind_direct_pattern(&patterns[0], payload, arm_value, replacements);
    }
    let TypedExpressionKind::Tuple(values) = &payload.kind else {
        return false;
    };
    patterns.len() == values.len()
        && patterns
            .iter()
            .zip(values)
            .all(|(pattern, value)| bind_direct_pattern(pattern, value, arm_value, replacements))
}

fn bind_one_projected_pattern(
    pattern: &TypedPatternIr,
    payload: Option<&TypedExpressionIr>,
    projectable: bool,
    arm_value: &TypedExpressionIr,
    replacements: &mut BTreeMap<DefinitionId, TypedExpressionIr>,
) -> bool {
    if projectable {
        payload
            .is_some_and(|payload| bind_direct_pattern(pattern, payload, arm_value, replacements))
    } else {
        unavailable_pattern_is_safe(pattern, arm_value)
    }
}

fn bind_direct_pattern(
    pattern: &TypedPatternIr,
    value: &TypedExpressionIr,
    arm_value: &TypedExpressionIr,
    replacements: &mut BTreeMap<DefinitionId, TypedExpressionIr>,
) -> bool {
    let Some(known) = known_match_value(value, &MatchScopes::default()) else {
        return match &pattern.kind {
            TypedPatternKind::Wildcard => true,
            TypedPatternKind::Binding(definition) => {
                replacements.insert(*definition, value.clone());
                true
            }
            _ => false,
        };
    };
    pattern_matches_shape(pattern, &known.shape)
        && bind_known_pattern(pattern, &known, arm_value, replacements)
}

fn unavailable_pattern_is_safe(pattern: &TypedPatternIr, arm_value: &TypedExpressionIr) -> bool {
    match &pattern.kind {
        TypedPatternKind::Wildcard => true,
        TypedPatternKind::Binding(definition) => {
            !expression_references_definition(arm_value, *definition)
        }
        TypedPatternKind::Struct { fields, .. } => fields
            .iter()
            .all(|(_, pattern)| unavailable_pattern_is_safe(pattern, arm_value)),
        TypedPatternKind::Literal(_)
        | TypedPatternKind::Variant { .. }
        | TypedPatternKind::BuiltinVariant { .. } => false,
    }
}

fn expression_references_definition(
    expression: &TypedExpressionIr,
    definition: DefinitionId,
) -> bool {
    if matches!(
        expression.kind,
        TypedExpressionKind::Reference(candidate) if candidate == definition
    ) {
        return true;
    }
    let mut children = Vec::new();
    collect_children(expression, &mut children);
    children
        .into_iter()
        .any(|child| expression_references_definition(child, definition))
}

fn replace_pattern_bindings(
    expression: &mut TypedExpressionIr,
    replacements: &BTreeMap<DefinitionId, TypedExpressionIr>,
    context: &mut PassContext,
) {
    if let TypedExpressionKind::Reference(definition) = expression.kind
        && let Some(replacement) = replacements.get(&definition)
    {
        let use_span = expression.span.clone();
        *expression = replacement.clone();
        expression.span = use_span;
        context.rewrites = context.rewrites.saturating_add(1);
        return;
    }
    for_each_child_expression(expression, &mut |child| {
        replace_pattern_bindings(child, replacements, context);
    });
}

fn is_match_specialization_safe(expression: &TypedExpressionIr) -> bool {
    match &expression.kind {
        TypedExpressionKind::Literal(_) | TypedExpressionKind::Reference(_) => true,
        TypedExpressionKind::Unary { operand, .. } => is_match_specialization_safe(operand),
        TypedExpressionKind::Binary {
            operator,
            left,
            right,
        } => {
            !matches!(operator, BinaryOperator::Divide | BinaryOperator::Remainder)
                && expression.ty != IrType::String
                && is_match_specialization_safe(left)
                && is_match_specialization_safe(right)
        }
        TypedExpressionKind::Construct { fields, .. } => fields
            .iter()
            .all(|(_, value)| is_match_specialization_safe(value)),
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            payload.as_deref().is_none_or(is_match_specialization_safe)
        }
        TypedExpressionKind::Tuple(values) => values.iter().all(is_match_specialization_safe),
        _ => false,
    }
}

/// WP41 plus the IR half of WP42: removes provably dead pure work.
///
/// An expression is deletable only when evaluating it can neither trap,
/// allocate, call the host, nor touch any resource: division stays because
/// of `DivideByZero`, constructors stay because heap admission can trap,
/// calls stay entirely. Constant `if` conditions select their branch,
/// `while false` loops disappear, and statements after a terminator are
/// unreachable and dropped. One bounded sweep, no fixpoint iteration.
pub struct DeadCodeElimination;

impl TypedIrPass for DeadCodeElimination {
    fn name(&self) -> &'static str {
        "dead-code-elimination"
    }

    fn run_function(&self, function: &mut TypedFunctionIr, context: &mut PassContext) {
        let mut uses = BTreeMap::new();
        count_block_references(&function.body, &mut uses);
        eliminate_in_block(&mut function.body, &uses, context);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PropagationMode {
    Constants,
    Copies,
}

#[derive(Clone)]
enum BoundValue {
    Literal(IrLiteral),
    Alias(DefinitionId),
}

struct PropagationScopes {
    scopes: Vec<BTreeMap<DefinitionId, BoundValue>>,
    /// Definitions proven immutable: parameters and non-`mut` lets.
    immutable: BTreeSet<DefinitionId>,
}

impl PropagationScopes {
    fn new(function: &TypedFunctionIr) -> Self {
        Self {
            scopes: vec![BTreeMap::new()],
            immutable: function.parameters.iter().copied().collect(),
        }
    }

    fn lookup(&self, definition: DefinitionId) -> Option<&BoundValue> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&definition))
    }

    fn bind(&mut self, definition: DefinitionId, value: BoundValue) {
        self.scopes
            .last_mut()
            .expect("propagation always keeps one scope")
            .insert(definition, value);
    }

    /// Defensive invalidation: analysis already rejects writes to immutable
    /// bindings, but a wrong propagation would be silent miscompilation.
    fn invalidate(&mut self, definition: DefinitionId) {
        self.immutable.remove(&definition);
        for scope in &mut self.scopes {
            scope.remove(&definition);
            scope.retain(
                |_, value| !matches!(value, BoundValue::Alias(source) if *source == definition),
            );
        }
    }
}

fn propagate_block(
    block: &mut TypedBlockIr,
    scopes: &mut PropagationScopes,
    mode: PropagationMode,
    context: &mut PassContext,
) {
    scopes.scopes.push(BTreeMap::new());
    for statement in &mut block.statements {
        propagate_statement(statement, scopes, mode, context);
    }
    if let Some(tail) = &mut block.tail {
        substitute_expression(tail, scopes, mode, context);
    }
    scopes.scopes.pop();
}

fn propagate_statement(
    statement: &mut TypedStatementIr,
    scopes: &mut PropagationScopes,
    mode: PropagationMode,
    context: &mut PassContext,
) {
    match statement {
        TypedStatementIr::Let {
            definition,
            mutable,
            value,
        } => {
            if let Some(value) = value {
                substitute_expression(value, scopes, mode, context);
            }
            if !*mutable {
                scopes.immutable.insert(*definition);
                if let Some(value) = value {
                    match (mode, &value.kind) {
                        (PropagationMode::Constants, TypedExpressionKind::Literal(literal)) => {
                            scopes.bind(*definition, BoundValue::Literal(literal.clone()));
                        }
                        (PropagationMode::Copies, TypedExpressionKind::Reference(source))
                            if scopes.immutable.contains(source) =>
                        {
                            // Resolve alias chains so every rewrite lands on
                            // the original binding.
                            let root = match scopes.lookup(*source) {
                                Some(BoundValue::Alias(root)) => *root,
                                _ => *source,
                            };
                            scopes.bind(*definition, BoundValue::Alias(root));
                        }
                        _ => {}
                    }
                }
            }
        }
        TypedStatementIr::Assign { target, value } => {
            substitute_place(target, scopes, mode, context);
            substitute_expression(value, scopes, mode, context);
            if let Some(root) = place_root_definition(target) {
                scopes.invalidate(root);
            }
        }
        TypedStatementIr::Expression(expression) => {
            substitute_expression(expression, scopes, mode, context);
        }
        TypedStatementIr::Return(value) => {
            if let Some(value) = value {
                substitute_expression(value, scopes, mode, context);
            }
        }
        TypedStatementIr::If {
            condition,
            then_block,
            else_block,
        } => {
            substitute_expression(condition, scopes, mode, context);
            propagate_block(then_block, scopes, mode, context);
            if let Some(else_block) = else_block {
                propagate_block(else_block, scopes, mode, context);
            }
        }
        TypedStatementIr::While {
            condition, body, ..
        } => {
            substitute_expression(condition, scopes, mode, context);
            propagate_block(body, scopes, mode, context);
        }
        TypedStatementIr::StaticRangeFor {
            start, end, body, ..
        } => {
            substitute_expression(start, scopes, mode, context);
            substitute_expression(end, scopes, mode, context);
            propagate_block(body, scopes, mode, context);
        }
        TypedStatementIr::Defer { captures, .. } => {
            for capture in captures {
                substitute_expression(capture, scopes, mode, context);
            }
        }
        TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield { .. } => {}
    }
}

const fn place_root_definition(place: &TypedPlaceIr) -> Option<DefinitionId> {
    match place {
        TypedPlaceIr::Definition(definition) => Some(*definition),
        TypedPlaceIr::Field { base, .. } => place_root_definition(base),
        TypedPlaceIr::ClassField { .. }
        | TypedPlaceIr::Index { .. }
        | TypedPlaceIr::StateField { .. } => None,
    }
}

fn substitute_place(
    place: &mut TypedPlaceIr,
    scopes: &mut PropagationScopes,
    mode: PropagationMode,
    context: &mut PassContext,
) {
    match place {
        TypedPlaceIr::Definition(_) => {}
        TypedPlaceIr::Field { base, .. } => substitute_place(base, scopes, mode, context),
        TypedPlaceIr::ClassField { object, .. } | TypedPlaceIr::StateField { base: object, .. } => {
            substitute_expression(object, scopes, mode, context);
        }
        TypedPlaceIr::Index { base, index } => {
            substitute_expression(base, scopes, mode, context);
            substitute_expression(index, scopes, mode, context);
        }
    }
}

fn substitute_expression(
    expression: &mut TypedExpressionIr,
    scopes: &mut PropagationScopes,
    mode: PropagationMode,
    context: &mut PassContext,
) {
    if let TypedExpressionKind::Reference(definition) = &expression.kind {
        match (mode, scopes.lookup(*definition)) {
            (PropagationMode::Constants, Some(BoundValue::Literal(literal))) => {
                expression.kind = TypedExpressionKind::Literal(literal.clone());
                context.rewrites = context.rewrites.saturating_add(1);
                return;
            }
            (PropagationMode::Copies, Some(BoundValue::Alias(source))) => {
                expression.kind = TypedExpressionKind::Reference(*source);
                context.rewrites = context.rewrites.saturating_add(1);
                return;
            }
            (PropagationMode::Constants, None) => {
                let mut visiting = BTreeSet::new();
                if let Some(mut replacement) = resolved_package_constant(
                    *definition,
                    &context.invariant,
                    &mut visiting,
                    MAX_PROPAGATED_CONSTANT_NODES,
                ) {
                    replacement.span = expression.span.clone();
                    *expression = replacement;
                    context.rewrites = context.rewrites.saturating_add(1);
                }
                return;
            }
            _ => return,
        }
    }
    for_each_child_expression(expression, &mut |child| {
        substitute_expression(child, scopes, mode, context);
    });
}

const MAX_PROPAGATED_CONSTANT_NODES: usize = 64;

fn resolved_package_constant(
    definition: DefinitionId,
    invariant: &PassInvariant,
    visiting: &mut BTreeSet<DefinitionId>,
    budget: usize,
) -> Option<TypedExpressionIr> {
    if budget == 0 || !visiting.insert(definition) {
        return None;
    }
    let mut expression = invariant.constant(definition)?.clone();
    let resolved = resolve_constant_references(&mut expression, invariant, visiting, budget);
    visiting.remove(&definition);
    resolved.then_some(expression)
}

fn resolve_constant_references(
    expression: &mut TypedExpressionIr,
    invariant: &PassInvariant,
    visiting: &mut BTreeSet<DefinitionId>,
    budget: usize,
) -> bool {
    if budget == 0 {
        return false;
    }
    if let TypedExpressionKind::Reference(definition) = expression.kind {
        let Some(mut replacement) =
            resolved_package_constant(definition, invariant, visiting, budget.saturating_sub(1))
        else {
            return false;
        };
        replacement.span = expression.span.clone();
        *expression = replacement;
        return true;
    }
    let allowed = match &expression.kind {
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Unary { .. }
        | TypedExpressionKind::Binary { .. }
        | TypedExpressionKind::Construct { .. }
        | TypedExpressionKind::EnumConstruct { .. }
        | TypedExpressionKind::BuiltinVariant { .. }
        | TypedExpressionKind::Field { .. }
        | TypedExpressionKind::Tuple(_)
        | TypedExpressionKind::Match { .. } => true,
        TypedExpressionKind::Reference(_)
        | TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::Call { .. }
        | TypedExpressionKind::StandardCall { .. }
        | TypedExpressionKind::BuiltinCall { .. }
        | TypedExpressionKind::HostCall { .. }
        | TypedExpressionKind::ClassConstruct { .. }
        | TypedExpressionKind::StateField { .. }
        | TypedExpressionKind::Index { .. }
        | TypedExpressionKind::Array(_)
        | TypedExpressionKind::StringInterpolation(_)
        | TypedExpressionKind::Try(_)
        | TypedExpressionKind::Update { .. }
        | TypedExpressionKind::Migration(_)
        | TypedExpressionKind::Await(_)
        | TypedExpressionKind::Yield => false,
    };
    if !allowed {
        return false;
    }
    let mut valid = true;
    for_each_child_expression(expression, &mut |child| {
        valid &= resolve_constant_references(child, invariant, visiting, budget.saturating_sub(1));
    });
    valid && expression_node_count(expression) <= budget
}

fn fold_block(block: &mut TypedBlockIr, context: &mut PassContext) {
    for statement in &mut block.statements {
        fold_statement(statement, context);
    }
    if let Some(tail) = &mut block.tail {
        fold_expression(tail, context);
    }
}

fn fold_statement(statement: &mut TypedStatementIr, context: &mut PassContext) {
    match statement {
        TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
            if let Some(value) = value {
                fold_expression(value, context);
            }
        }
        TypedStatementIr::Assign { target, value } => {
            fold_place(target, context);
            fold_expression(value, context);
        }
        TypedStatementIr::Expression(expression) => fold_expression(expression, context),
        TypedStatementIr::If {
            condition,
            then_block,
            else_block,
        } => {
            fold_expression(condition, context);
            fold_block(then_block, context);
            if let Some(else_block) = else_block {
                fold_block(else_block, context);
            }
        }
        TypedStatementIr::While {
            condition, body, ..
        } => {
            fold_expression(condition, context);
            fold_block(body, context);
        }
        TypedStatementIr::StaticRangeFor {
            start, end, body, ..
        } => {
            fold_expression(start, context);
            fold_expression(end, context);
            fold_block(body, context);
        }
        TypedStatementIr::Defer { captures, .. } => {
            for capture in captures {
                fold_expression(capture, context);
            }
        }
        TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield { .. } => {}
    }
}

fn fold_place(place: &mut TypedPlaceIr, context: &mut PassContext) {
    match place {
        TypedPlaceIr::Definition(_) => {}
        TypedPlaceIr::Field { base, .. } => fold_place(base, context),
        TypedPlaceIr::ClassField { object, .. } | TypedPlaceIr::StateField { base: object, .. } => {
            fold_expression(object, context);
        }
        TypedPlaceIr::Index { base, index } => {
            fold_expression(base, context);
            fold_expression(index, context);
        }
    }
}

#[allow(clippy::too_many_lines)]
fn fold_expression(expression: &mut TypedExpressionIr, context: &mut PassContext) {
    match &mut expression.kind {
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Reference(_)
        | TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::Yield => {}
        TypedExpressionKind::Unary { operand, .. } => fold_expression(operand, context),
        TypedExpressionKind::Binary { left, right, .. } => {
            fold_expression(left, context);
            fold_expression(right, context);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => {
            for argument in arguments {
                fold_expression(argument, context);
            }
        }
        TypedExpressionKind::Construct { fields, .. } => {
            for (_, field) in fields {
                fold_expression(field, context);
            }
        }
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            for (_, field) in fields {
                fold_expression(field, context);
            }
            if let Some(update) = update {
                fold_expression(update, context);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                fold_expression(payload, context);
            }
        }
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            fold_expression(base, context);
        }
        TypedExpressionKind::Index { base, index } => {
            fold_expression(base, context);
            fold_expression(index, context);
        }
        TypedExpressionKind::Array(items)
        | TypedExpressionKind::Tuple(items)
        | TypedExpressionKind::StringInterpolation(items) => {
            for item in items {
                fold_expression(item, context);
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            fold_expression(value, context);
            for arm in arms {
                fold_expression(&mut arm.value, context);
            }
        }
        TypedExpressionKind::Try(inner) | TypedExpressionKind::Await(inner) => {
            fold_expression(inner, context);
        }
        TypedExpressionKind::Update { base, fields } => {
            fold_expression(base, context);
            for (_, field) in fields {
                fold_expression(field, context);
            }
        }
        TypedExpressionKind::Migration(intrinsic) => fold_migration(intrinsic, context),
    }
    if let Some(mut projected) = folded_field_projection(expression) {
        projected.span = expression.span.clone();
        *expression = projected;
        context.rewrites = context.rewrites.saturating_add(1);
        return;
    }
    if let Some(folded) = folded_literal(expression) {
        expression.kind = TypedExpressionKind::Literal(folded);
        context.rewrites = context.rewrites.saturating_add(1);
    }
}

fn folded_field_projection(expression: &TypedExpressionIr) -> Option<TypedExpressionIr> {
    let TypedExpressionKind::Field { base, field } = &expression.kind else {
        return None;
    };
    let TypedExpressionKind::Construct { fields, .. } = &base.kind else {
        return None;
    };
    if !fields
        .iter()
        .all(|(_, value)| is_match_specialization_safe(value))
    {
        return None;
    }
    fields
        .iter()
        .find(|(candidate, _)| candidate == field)
        .map(|(_, value)| value.clone())
}

fn fold_migration(intrinsic: &mut MigrationIntrinsicIr, context: &mut PassContext) {
    match intrinsic {
        MigrationIntrinsicIr::OldFieldGet { object, .. } => fold_expression(object, context),
        MigrationIntrinsicIr::NewSet { object, value, .. } => {
            fold_expression(object, context);
            fold_expression(value, context);
        }
        MigrationIntrinsicIr::Replace { target, .. } => fold_expression(target, context),
        MigrationIntrinsicIr::OldGet { .. }
        | MigrationIntrinsicIr::NewCreate { .. }
        | MigrationIntrinsicIr::Preserve { .. }
        | MigrationIntrinsicIr::Delete { .. }
        | MigrationIntrinsicIr::Finish => {}
    }
}

/// Applies `visit` to every direct child expression of `expression`.
fn for_each_child_expression(
    expression: &mut TypedExpressionIr,
    visit: &mut impl FnMut(&mut TypedExpressionIr),
) {
    match &mut expression.kind {
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Reference(_)
        | TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::Yield => {}
        TypedExpressionKind::Unary { operand, .. } => visit(operand),
        TypedExpressionKind::Binary { left, right, .. } => {
            visit(left);
            visit(right);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => {
            for argument in arguments {
                visit(argument);
            }
        }
        TypedExpressionKind::Construct { fields, .. } => {
            for (_, field) in fields {
                visit(field);
            }
        }
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            for (_, field) in fields {
                visit(field);
            }
            if let Some(update) = update {
                visit(update);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                visit(payload);
            }
        }
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            visit(base);
        }
        TypedExpressionKind::Index { base, index } => {
            visit(base);
            visit(index);
        }
        TypedExpressionKind::Array(items)
        | TypedExpressionKind::Tuple(items)
        | TypedExpressionKind::StringInterpolation(items) => {
            for item in items {
                visit(item);
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            visit(value);
            for arm in arms {
                visit(&mut arm.value);
            }
        }
        TypedExpressionKind::Try(inner) | TypedExpressionKind::Await(inner) => visit(inner),
        TypedExpressionKind::Update { base, fields } => {
            visit(base);
            for (_, field) in fields {
                visit(field);
            }
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. } => visit(object),
            MigrationIntrinsicIr::NewSet { object, value, .. } => {
                visit(object);
                visit(value);
            }
            MigrationIntrinsicIr::Replace { target, .. } => visit(target),
            MigrationIntrinsicIr::OldGet { .. }
            | MigrationIntrinsicIr::NewCreate { .. }
            | MigrationIntrinsicIr::Preserve { .. }
            | MigrationIntrinsicIr::Delete { .. }
            | MigrationIntrinsicIr::Finish => {}
        },
    }
}

/// Counts every `Reference` occurrence so WP41 only removes bindings that
/// are provably unused.
fn count_block_references(block: &TypedBlockIr, uses: &mut BTreeMap<DefinitionId, usize>) {
    for statement in &block.statements {
        count_statement_references(statement, uses);
    }
    if let Some(tail) = &block.tail {
        count_expression_references(tail, uses);
    }
}

fn count_statement_references(
    statement: &TypedStatementIr,
    uses: &mut BTreeMap<DefinitionId, usize>,
) {
    match statement {
        TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
            if let Some(value) = value {
                count_expression_references(value, uses);
            }
        }
        TypedStatementIr::Assign { target, value } => {
            count_place_references(target, uses);
            count_expression_references(value, uses);
        }
        TypedStatementIr::Expression(expression) => count_expression_references(expression, uses),
        TypedStatementIr::If {
            condition,
            then_block,
            else_block,
        } => {
            count_expression_references(condition, uses);
            count_block_references(then_block, uses);
            if let Some(else_block) = else_block {
                count_block_references(else_block, uses);
            }
        }
        TypedStatementIr::While {
            condition, body, ..
        } => {
            count_expression_references(condition, uses);
            count_block_references(body, uses);
        }
        TypedStatementIr::StaticRangeFor {
            start, end, body, ..
        } => {
            count_expression_references(start, uses);
            count_expression_references(end, uses);
            count_block_references(body, uses);
        }
        TypedStatementIr::Defer { captures, .. } => {
            for capture in captures {
                count_expression_references(capture, uses);
            }
        }
        TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield { .. } => {}
    }
}

fn count_place_references(place: &TypedPlaceIr, uses: &mut BTreeMap<DefinitionId, usize>) {
    match place {
        // Assignment through a binding keeps that binding alive.
        TypedPlaceIr::Definition(definition) => {
            *uses.entry(*definition).or_default() += 1;
        }
        TypedPlaceIr::Field { base, .. } => count_place_references(base, uses),
        TypedPlaceIr::ClassField { object, .. } | TypedPlaceIr::StateField { base: object, .. } => {
            count_expression_references(object, uses);
        }
        TypedPlaceIr::Index { base, index } => {
            count_expression_references(base, uses);
            count_expression_references(index, uses);
        }
    }
}

fn count_expression_references(
    expression: &TypedExpressionIr,
    uses: &mut BTreeMap<DefinitionId, usize>,
) {
    if let TypedExpressionKind::Reference(definition) = &expression.kind {
        *uses.entry(*definition).or_default() += 1;
        return;
    }
    // SAFETY of the shortcut: the mutable walker never mutates through the
    // visitor we pass here; it only needs unique access structurally. To
    // keep one authoritative child enumeration we clone nothing and reuse
    // the mutable walker through interior recursion instead.
    let mut children: Vec<&TypedExpressionIr> = Vec::new();
    collect_children(expression, &mut children);
    for child in children {
        count_expression_references(child, uses);
    }
}

/// Shared read-only child enumeration mirroring `for_each_child_expression`.
fn collect_children<'expr>(
    expression: &'expr TypedExpressionIr,
    children: &mut Vec<&'expr TypedExpressionIr>,
) {
    match &expression.kind {
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Reference(_)
        | TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::Yield => {}
        TypedExpressionKind::Unary { operand, .. } => children.push(operand),
        TypedExpressionKind::Binary { left, right, .. } => {
            children.push(left);
            children.push(right);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => children.extend(arguments.iter()),
        TypedExpressionKind::Construct { fields, .. } => {
            children.extend(fields.iter().map(|(_, field)| field));
        }
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            children.extend(fields.iter().map(|(_, field)| field));
            if let Some(update) = update {
                children.push(update);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                children.push(payload);
            }
        }
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            children.push(base);
        }
        TypedExpressionKind::Index { base, index } => {
            children.push(base);
            children.push(index);
        }
        TypedExpressionKind::Array(items)
        | TypedExpressionKind::Tuple(items)
        | TypedExpressionKind::StringInterpolation(items) => children.extend(items.iter()),
        TypedExpressionKind::Match { value, arms } => {
            children.push(value);
            children.extend(arms.iter().map(|arm| &arm.value));
        }
        TypedExpressionKind::Try(inner) | TypedExpressionKind::Await(inner) => {
            children.push(inner);
        }
        TypedExpressionKind::Update { base, fields } => {
            children.push(base);
            children.extend(fields.iter().map(|(_, field)| field));
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. } => children.push(object),
            MigrationIntrinsicIr::NewSet { object, value, .. } => {
                children.push(object);
                children.push(value);
            }
            MigrationIntrinsicIr::Replace { target, .. } => children.push(target),
            MigrationIntrinsicIr::OldGet { .. }
            | MigrationIntrinsicIr::NewCreate { .. }
            | MigrationIntrinsicIr::Preserve { .. }
            | MigrationIntrinsicIr::Delete { .. }
            | MigrationIntrinsicIr::Finish => {}
        },
    }
}

/// True only when evaluating the expression can neither trap, allocate,
/// suspend, nor touch host or state resources: dropping it is unobservable.
fn is_pure_expression(expression: &TypedExpressionIr) -> bool {
    match &expression.kind {
        TypedExpressionKind::Literal(_) | TypedExpressionKind::Reference(_) => true,
        TypedExpressionKind::Unary { operand, .. } => is_pure_expression(operand),
        TypedExpressionKind::Binary {
            operator,
            left,
            right,
        } => {
            // Integer arithmetic wraps instead of trapping, so it is pure;
            // division and remainder can raise DivideByZero and must stay.
            // Dynamic String operations may allocate or consume
            // length-dependent deterministic work, so they are not dead-code
            // candidates even though they share the Binary IR node.
            !matches!(operator, BinaryOperator::Divide | BinaryOperator::Remainder)
                && expression.ty != IrType::String
                && left.ty != IrType::String
                && right.ty != IrType::String
                && is_pure_expression(left)
                && is_pure_expression(right)
        }
        _ => false,
    }
}

fn eliminate_in_block(
    block: &mut TypedBlockIr,
    uses: &BTreeMap<DefinitionId, usize>,
    context: &mut PassContext,
) {
    let statements = std::mem::take(&mut block.statements);
    let mut rebuilt = Vec::with_capacity(statements.len());
    let mut terminated = false;
    for mut statement in statements {
        if terminated {
            // Unreachable after Return/Break/Continue (WP41).
            context.rewrites = context.rewrites.saturating_add(1);
            continue;
        }
        match &mut statement {
            TypedStatementIr::Let {
                definition, value, ..
            } => {
                let unused = uses.get(definition).copied().unwrap_or(0) == 0;
                let removable = unused && value.as_ref().is_none_or(is_pure_expression);
                if removable {
                    context.rewrites = context.rewrites.saturating_add(1);
                    continue;
                }
            }
            TypedStatementIr::Expression(expression) => {
                if is_pure_expression(expression) {
                    context.rewrites = context.rewrites.saturating_add(1);
                    continue;
                }
            }
            TypedStatementIr::If {
                condition: _,
                then_block,
                else_block,
            } => {
                // Constant-condition branch selection is deliberately NOT
                // performed here: acceptability checks (missing-return flow
                // analysis) run during lowering, and flattening `if true {
                // return x; }` would turn a rejected program into an
                // accepted one. Branch selection moves to the post-flow
                // ExecutableModule stage (WP59+) where acceptability is
                // already settled.
                eliminate_in_block(then_block, uses, context);
                if let Some(else_block) = else_block {
                    eliminate_in_block(else_block, uses, context);
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                if matches!(
                    &condition.kind,
                    TypedExpressionKind::Literal(IrLiteral::Bool(false))
                ) {
                    // WP42 (IR side): a false loop never runs.
                    context.rewrites = context.rewrites.saturating_add(1);
                    continue;
                }
                eliminate_in_block(body, uses, context);
            }
            TypedStatementIr::StaticRangeFor { body, .. } => {
                eliminate_in_block(body, uses, context);
            }
            TypedStatementIr::Return(_) | TypedStatementIr::Break | TypedStatementIr::Continue => {
                rebuilt.push(statement);
                terminated = true;
                continue;
            }
            TypedStatementIr::Assign { .. }
            | TypedStatementIr::Defer { .. }
            | TypedStatementIr::Yield { .. } => {}
        }
        rebuilt.push(statement);
    }
    block.statements = rebuilt;
}

/// The literal replacing `expression`, when folding cannot change any
/// observable runtime behavior.
fn folded_literal(expression: &TypedExpressionIr) -> Option<IrLiteral> {
    match &expression.kind {
        TypedExpressionKind::Unary { operator, operand } => {
            let operand = literal_of(operand)?;
            fold_unary(*operator, operand)
        }
        TypedExpressionKind::Binary {
            operator,
            left,
            right,
        } => {
            let left = literal_of(left)?;
            let right = literal_of(right)?;
            fold_binary(*operator, left, right)
        }
        _ => None,
    }
}

fn literal_of(expression: &TypedExpressionIr) -> Option<&IrLiteral> {
    match &expression.kind {
        TypedExpressionKind::Literal(literal) => Some(literal),
        _ => None,
    }
}

fn fold_unary(operator: UnaryOperator, operand: &IrLiteral) -> Option<IrLiteral> {
    match (operator, operand) {
        // checked_neg refuses i32::MIN/i64::MIN; the runtime's overflow
        // behavior stays authoritative there.
        (UnaryOperator::Negate, IrLiteral::I32(value)) => value.checked_neg().map(IrLiteral::I32),
        (UnaryOperator::Negate, IrLiteral::I64(value)) => value.checked_neg().map(IrLiteral::I64),
        (UnaryOperator::Negate, IrLiteral::F32(value)) => Some(IrLiteral::F32(-value)),
        (UnaryOperator::Negate, IrLiteral::F64(value)) => Some(IrLiteral::F64(-value)),
        (UnaryOperator::Not, IrLiteral::Bool(value)) => Some(IrLiteral::Bool(!value)),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
// Strict float comparison is intentional: folds must reproduce the
// interpreter's IEEE comparison semantics bit for bit, not approximate them.
#[allow(clippy::float_cmp)]
fn fold_binary(operator: BinaryOperator, left: &IrLiteral, right: &IrLiteral) -> Option<IrLiteral> {
    use BinaryOperator as Op;
    use IrLiteral as Lit;
    Some(match (left, right) {
        (Lit::I32(lhs), Lit::I32(rhs)) => match operator {
            Op::Add => Lit::I32(lhs.checked_add(*rhs)?),
            Op::Subtract => Lit::I32(lhs.checked_sub(*rhs)?),
            Op::Multiply => Lit::I32(lhs.checked_mul(*rhs)?),
            // Division and remainder stay runtime work: zero divisors and
            // MIN/-1 must produce the interpreter's trap, not a fold.
            Op::Divide => Lit::I32((*rhs != 0).then(|| lhs.checked_div(*rhs))??),
            Op::Remainder => Lit::I32((*rhs != 0).then(|| lhs.checked_rem(*rhs))??),
            Op::Equal => Lit::Bool(lhs == rhs),
            Op::NotEqual => Lit::Bool(lhs != rhs),
            Op::Less => Lit::Bool(lhs < rhs),
            Op::LessEqual => Lit::Bool(lhs <= rhs),
            Op::Greater => Lit::Bool(lhs > rhs),
            Op::GreaterEqual => Lit::Bool(lhs >= rhs),
            Op::And | Op::Or => return None,
        },
        (Lit::I64(lhs), Lit::I64(rhs)) => match operator {
            Op::Add => Lit::I64(lhs.checked_add(*rhs)?),
            Op::Subtract => Lit::I64(lhs.checked_sub(*rhs)?),
            Op::Multiply => Lit::I64(lhs.checked_mul(*rhs)?),
            Op::Divide => Lit::I64((*rhs != 0).then(|| lhs.checked_div(*rhs))??),
            Op::Remainder => Lit::I64((*rhs != 0).then(|| lhs.checked_rem(*rhs))??),
            Op::Equal => Lit::Bool(lhs == rhs),
            Op::NotEqual => Lit::Bool(lhs != rhs),
            Op::Less => Lit::Bool(lhs < rhs),
            Op::LessEqual => Lit::Bool(lhs <= rhs),
            Op::Greater => Lit::Bool(lhs > rhs),
            Op::GreaterEqual => Lit::Bool(lhs >= rhs),
            Op::And | Op::Or => return None,
        },
        (Lit::F32(lhs), Lit::F32(rhs)) => match operator {
            // IEEE add/sub/mul are deterministic; division is left to the
            // runtime until its zero-divisor semantics are frozen by the
            // differential gate.
            Op::Add => Lit::F32(lhs + rhs),
            Op::Subtract => Lit::F32(lhs - rhs),
            Op::Multiply => Lit::F32(lhs * rhs),
            Op::Equal => Lit::Bool(lhs == rhs),
            Op::NotEqual => Lit::Bool(lhs != rhs),
            Op::Less => Lit::Bool(lhs < rhs),
            Op::LessEqual => Lit::Bool(lhs <= rhs),
            Op::Greater => Lit::Bool(lhs > rhs),
            Op::GreaterEqual => Lit::Bool(lhs >= rhs),
            Op::Divide | Op::Remainder | Op::And | Op::Or => return None,
        },
        (Lit::F64(lhs), Lit::F64(rhs)) => match operator {
            Op::Add => Lit::F64(lhs + rhs),
            Op::Subtract => Lit::F64(lhs - rhs),
            Op::Multiply => Lit::F64(lhs * rhs),
            Op::Equal => Lit::Bool(lhs == rhs),
            Op::NotEqual => Lit::Bool(lhs != rhs),
            Op::Less => Lit::Bool(lhs < rhs),
            Op::LessEqual => Lit::Bool(lhs <= rhs),
            Op::Greater => Lit::Bool(lhs > rhs),
            Op::GreaterEqual => Lit::Bool(lhs >= rhs),
            Op::Divide | Op::Remainder | Op::And | Op::Or => return None,
        },
        (Lit::Bool(lhs), Lit::Bool(rhs)) => match operator {
            Op::And => Lit::Bool(*lhs && *rhs),
            Op::Or => Lit::Bool(*lhs || *rhs),
            Op::Equal => Lit::Bool(lhs == rhs),
            Op::NotEqual => Lit::Bool(lhs != rhs),
            _ => return None,
        },
        (Lit::Rune(lhs), Lit::Rune(rhs)) => match operator {
            Op::Equal => Lit::Bool(lhs == rhs),
            Op::NotEqual => Lit::Bool(lhs != rhs),
            _ => return None,
        },
        // Folded concatenations become module constants: with the WP56
        // interned pool a `LoadString` of the combined content allocates
        // nothing new, exactly as if the source had written the combined
        // literal. Oversized results stay runtime work so the pass cannot
        // bloat the constant table, and string ordering comparisons stay
        // runtime work because the interpreter never defines them.
        (Lit::String(lhs), Lit::String(rhs)) => match operator {
            Op::Add => {
                let length = lhs.len().checked_add(rhs.len())?;
                if length > MAX_FOLDED_STRING_BYTES {
                    return None;
                }
                let mut value = String::with_capacity(length);
                value.push_str(lhs);
                value.push_str(rhs);
                Lit::String(value)
            }
            Op::Equal => Lit::Bool(lhs == rhs),
            Op::NotEqual => Lit::Bool(lhs != rhs),
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{NormalizedPackagePath, PackageId, SourceKey};
    use crate::ir::{DefinitionId, IrEffect, IrType, SourceRange};

    fn expr(kind: TypedExpressionKind, ty: IrType) -> TypedExpressionIr {
        TypedExpressionIr {
            ty,
            effect: IrEffect::Ordinary,
            span: SourceRange {
                source: SourceKey::new(
                    PackageId::new("test.package").unwrap(),
                    NormalizedPackagePath::new("main.nexa").unwrap(),
                ),
                start: 0,
                end: 0,
            },
            kind,
        }
    }

    fn literal(value: IrLiteral, ty: IrType) -> Box<TypedExpressionIr> {
        Box::new(expr(TypedExpressionKind::Literal(value), ty))
    }

    fn binary(
        operator: BinaryOperator,
        left: Box<TypedExpressionIr>,
        right: Box<TypedExpressionIr>,
        ty: IrType,
    ) -> TypedExpressionIr {
        expr(
            TypedExpressionKind::Binary {
                operator,
                left,
                right,
            },
            ty,
        )
    }

    fn function_returning(expression: TypedExpressionIr) -> TypedFunctionIr {
        TypedFunctionIr {
            parameters: Vec::new(),
            locals: Vec::new(),
            return_type: expression.ty.clone(),
            effect: IrEffect::Ordinary,
            body: TypedBlockIr {
                statements: vec![TypedStatementIr::Return(Some(expression))],
                tail: None,
            },
        }
    }

    fn folded_return(function: &TypedFunctionIr) -> &TypedExpressionKind {
        let TypedStatementIr::Return(Some(expression)) = &function.body.statements[0] else {
            panic!("return statement survives folding");
        };
        &expression.kind
    }

    #[test]
    fn nested_arithmetic_and_comparison_fold_to_literals() {
        // (1 + 2 * 3) == 7
        let product = binary(
            BinaryOperator::Multiply,
            literal(IrLiteral::I32(2), IrType::I32),
            literal(IrLiteral::I32(3), IrType::I32),
            IrType::I32,
        );
        let sum = binary(
            BinaryOperator::Add,
            literal(IrLiteral::I32(1), IrType::I32),
            Box::new(product),
            IrType::I32,
        );
        let comparison = binary(
            BinaryOperator::Equal,
            Box::new(sum),
            literal(IrLiteral::I32(7), IrType::I32),
            IrType::Bool,
        );
        let mut function = function_returning(comparison);
        let reports = PassManager::standard().optimize_function(&mut function);
        assert_eq!(reports.len(), 7, "standard pipeline runs seven passes");
        assert_eq!(reports[0].pass, "constant-folding");
        assert_eq!(reports[0].rewrites, 3);
        assert_eq!(
            folded_return(&function),
            &TypedExpressionKind::Literal(IrLiteral::Bool(true))
        );
    }

    #[test]
    fn traps_overflow_and_float_division_stay_runtime_work() {
        let cases = [
            // Divide-by-zero must reach the interpreter as a trap.
            binary(
                BinaryOperator::Divide,
                literal(IrLiteral::I32(1), IrType::I32),
                literal(IrLiteral::I32(0), IrType::I32),
                IrType::I32,
            ),
            // MIN / -1 overflows: checked_div refuses, the runtime decides.
            binary(
                BinaryOperator::Divide,
                literal(IrLiteral::I32(i32::MIN), IrType::I32),
                literal(IrLiteral::I32(-1), IrType::I32),
                IrType::I32,
            ),
            // Integer overflow is runtime behavior, never folded.
            binary(
                BinaryOperator::Add,
                literal(IrLiteral::I32(i32::MAX), IrType::I32),
                literal(IrLiteral::I32(1), IrType::I32),
                IrType::I32,
            ),
            // Float division semantics wait for the differential gate.
            binary(
                BinaryOperator::Divide,
                literal(IrLiteral::F64(1.0), IrType::F64),
                literal(IrLiteral::F64(0.0), IrType::F64),
                IrType::F64,
            ),
        ];
        for case in cases {
            let original_kind = case.kind.clone();
            let mut function = function_returning(case);
            let reports = PassManager::standard().optimize_function(&mut function);
            assert_eq!(reports[0].rewrites, 0);
            assert_eq!(folded_return(&function), &original_kind);
        }
    }

    #[test]
    fn propagation_dce_and_branch_selection_work_end_to_end() {
        // let a = 2; let b = a; let dead = 9; if true { return b + 3; }
        let mut function = TypedFunctionIr {
            parameters: Vec::new(),
            locals: vec![DefinitionId(1), DefinitionId(2), DefinitionId(3)],
            return_type: IrType::I32,
            effect: IrEffect::Ordinary,
            body: TypedBlockIr {
                statements: vec![
                    TypedStatementIr::Let {
                        definition: DefinitionId(1),
                        mutable: false,
                        value: Some(*literal(IrLiteral::I32(2), IrType::I32)),
                    },
                    TypedStatementIr::Let {
                        definition: DefinitionId(2),
                        mutable: false,
                        value: Some(expr(
                            TypedExpressionKind::Reference(DefinitionId(1)),
                            IrType::I32,
                        )),
                    },
                    TypedStatementIr::Let {
                        definition: DefinitionId(3),
                        mutable: false,
                        value: Some(*literal(IrLiteral::I32(9), IrType::I32)),
                    },
                    TypedStatementIr::If {
                        condition: *literal(IrLiteral::Bool(true), IrType::Bool),
                        then_block: TypedBlockIr {
                            statements: vec![TypedStatementIr::Return(Some(binary(
                                BinaryOperator::Add,
                                Box::new(expr(
                                    TypedExpressionKind::Reference(DefinitionId(2)),
                                    IrType::I32,
                                )),
                                literal(IrLiteral::I32(3), IrType::I32),
                                IrType::I32,
                            )))],
                            tail: None,
                        },
                        else_block: None,
                    },
                ],
                tail: None,
            },
        };
        let reports = PassManager::standard().optimize_function(&mut function);
        let total_rewrites: u64 = reports.iter().map(|report| report.rewrites).sum();
        assert!(total_rewrites >= 4, "reports: {reports:?}");
        // Constants propagate through the alias chain, the addition folds
        // inside the branch, and every dead binding disappears. The `if`
        // itself must survive: branch selection is deferred until after
        // acceptability flow analysis (see eliminate_in_block).
        assert_eq!(function.body.statements.len(), 1, "{:?}", function.body);
        let TypedStatementIr::If { then_block, .. } = &function.body.statements[0] else {
            panic!("constant branch survives the IR stage: {:?}", function.body);
        };
        let TypedStatementIr::Return(Some(value)) = &then_block.statements[0] else {
            panic!("branch keeps its return: {then_block:?}");
        };
        assert_eq!(value.kind, TypedExpressionKind::Literal(IrLiteral::I32(5)));
    }

    #[test]
    fn effectful_and_trapping_statements_survive_dce() {
        // let unused_div = 1 / 0;  -> can trap, must stay
        // while false { ... }      -> removed
        // return 0; let after = 1; -> unreachable tail removed
        let mut function = TypedFunctionIr {
            parameters: Vec::new(),
            locals: vec![DefinitionId(7), DefinitionId(8)],
            return_type: IrType::I32,
            effect: IrEffect::Ordinary,
            body: TypedBlockIr {
                statements: vec![
                    TypedStatementIr::Let {
                        definition: DefinitionId(7),
                        mutable: false,
                        value: Some(binary(
                            BinaryOperator::Divide,
                            literal(IrLiteral::I32(1), IrType::I32),
                            literal(IrLiteral::I32(0), IrType::I32),
                            IrType::I32,
                        )),
                    },
                    TypedStatementIr::While {
                        condition: *literal(IrLiteral::Bool(false), IrType::Bool),
                        body: TypedBlockIr {
                            statements: Vec::new(),
                            tail: None,
                        },
                        max_iterations: 8,
                    },
                    TypedStatementIr::Return(Some(*literal(IrLiteral::I32(0), IrType::I32))),
                    TypedStatementIr::Let {
                        definition: DefinitionId(8),
                        mutable: false,
                        value: Some(*literal(IrLiteral::I32(1), IrType::I32)),
                    },
                ],
                tail: None,
            },
        };
        PassManager::standard().optimize_function(&mut function);
        assert_eq!(function.body.statements.len(), 2, "{:?}", function.body);
        assert!(
            matches!(&function.body.statements[0], TypedStatementIr::Let { definition, .. } if *definition == DefinitionId(7)),
            "trapping division binding must survive"
        );
        assert!(matches!(
            &function.body.statements[1],
            TypedStatementIr::Return(_)
        ));
    }

    #[test]
    fn mutable_bindings_are_never_propagated() {
        // let mut m = 1; m = 2; return m;
        let mut function = TypedFunctionIr {
            parameters: Vec::new(),
            locals: vec![DefinitionId(4)],
            return_type: IrType::I32,
            effect: IrEffect::Ordinary,
            body: TypedBlockIr {
                statements: vec![
                    TypedStatementIr::Let {
                        definition: DefinitionId(4),
                        mutable: true,
                        value: Some(*literal(IrLiteral::I32(1), IrType::I32)),
                    },
                    TypedStatementIr::Assign {
                        target: TypedPlaceIr::Definition(DefinitionId(4)),
                        value: *literal(IrLiteral::I32(2), IrType::I32),
                    },
                    TypedStatementIr::Return(Some(expr(
                        TypedExpressionKind::Reference(DefinitionId(4)),
                        IrType::I32,
                    ))),
                ],
                tail: None,
            },
        };
        PassManager::standard().optimize_function(&mut function);
        let TypedStatementIr::Return(Some(value)) = function
            .body
            .statements
            .iter()
            .find(|statement| matches!(statement, TypedStatementIr::Return(_)))
            .expect("return survives")
        else {
            panic!("return has a value");
        };
        assert_eq!(
            value.kind,
            TypedExpressionKind::Reference(DefinitionId(4)),
            "mutable binding reads must stay runtime reads"
        );
    }

    #[test]
    fn string_concat_and_equality_fold_to_module_constants() {
        // "nexa" + "-benchmark" folds into one interned module constant.
        let concat = binary(
            BinaryOperator::Add,
            literal(IrLiteral::String("nexa".into()), IrType::String),
            literal(IrLiteral::String("-benchmark".into()), IrType::String),
            IrType::String,
        );
        let mut function = function_returning(concat);
        let reports = PassManager::standard().optimize_function(&mut function);
        assert_eq!(reports[0].rewrites, 1);
        assert_eq!(
            folded_return(&function),
            &TypedExpressionKind::Literal(IrLiteral::String("nexa-benchmark".into()))
        );

        let comparison = binary(
            BinaryOperator::Equal,
            literal(IrLiteral::String("left".into()), IrType::String),
            literal(IrLiteral::String("left".into()), IrType::String),
            IrType::Bool,
        );
        let mut function = function_returning(comparison);
        PassManager::standard().optimize_function(&mut function);
        assert_eq!(
            folded_return(&function),
            &TypedExpressionKind::Literal(IrLiteral::Bool(true))
        );
    }

    #[test]
    fn string_bindings_propagate_into_folds() {
        // let s = "x"; return s + "y"; -> return "xy";
        let mut function = TypedFunctionIr {
            parameters: Vec::new(),
            locals: vec![DefinitionId(6)],
            return_type: IrType::String,
            effect: IrEffect::Ordinary,
            body: TypedBlockIr {
                statements: vec![
                    TypedStatementIr::Let {
                        definition: DefinitionId(6),
                        mutable: false,
                        value: Some(*literal(IrLiteral::String("x".into()), IrType::String)),
                    },
                    TypedStatementIr::Return(Some(binary(
                        BinaryOperator::Add,
                        Box::new(expr(
                            TypedExpressionKind::Reference(DefinitionId(6)),
                            IrType::String,
                        )),
                        literal(IrLiteral::String("y".into()), IrType::String),
                        IrType::String,
                    ))),
                ],
                tail: None,
            },
        };
        PassManager::standard().optimize_function(&mut function);
        let TypedStatementIr::Return(Some(value)) = function
            .body
            .statements
            .iter()
            .find(|statement| matches!(statement, TypedStatementIr::Return(_)))
            .expect("return survives")
        else {
            panic!("return has a value");
        };
        assert_eq!(
            value.kind,
            TypedExpressionKind::Literal(IrLiteral::String("xy".into()))
        );
    }

    #[test]
    fn oversized_string_concat_stays_runtime_work() {
        let left = "a".repeat(MAX_FOLDED_STRING_BYTES / 2 + 1);
        let right = "b".repeat(MAX_FOLDED_STRING_BYTES / 2 + 1);
        let concat = binary(
            BinaryOperator::Add,
            literal(IrLiteral::String(left), IrType::String),
            literal(IrLiteral::String(right), IrType::String),
            IrType::String,
        );
        let original_kind = concat.kind.clone();
        let mut function = function_returning(concat);
        let reports = PassManager::standard().optimize_function(&mut function);
        assert_eq!(reports[0].rewrites, 0);
        assert_eq!(folded_return(&function), &original_kind);
    }

    #[test]
    fn folding_reaches_nested_statements_and_preserves_type() {
        let condition = binary(
            BinaryOperator::And,
            literal(IrLiteral::Bool(true), IrType::Bool),
            literal(IrLiteral::Bool(false), IrType::Bool),
            IrType::Bool,
        );
        let mut function = TypedFunctionIr {
            parameters: Vec::new(),
            locals: vec![DefinitionId(1)],
            return_type: IrType::Unit,
            effect: IrEffect::Ordinary,
            body: TypedBlockIr {
                statements: vec![TypedStatementIr::If {
                    condition,
                    then_block: TypedBlockIr {
                        statements: vec![TypedStatementIr::Let {
                            definition: DefinitionId(1),
                            mutable: false,
                            value: Some(binary(
                                BinaryOperator::Subtract,
                                literal(IrLiteral::I64(10), IrType::I64),
                                literal(IrLiteral::I64(4), IrType::I64),
                                IrType::I64,
                            )),
                        }],
                        tail: None,
                    },
                    else_block: None,
                }],
                tail: None,
            },
        };
        // Folding alone: the full pipeline would then legitimately delete
        // the constant-false branch, which the DCE tests cover separately.
        let mut context = PassContext::default();
        ConstantFolding.run_function(&mut function, &mut context);
        assert_eq!(context.rewrites, 2);
        let TypedStatementIr::If {
            condition,
            then_block,
            ..
        } = &function.body.statements[0]
        else {
            panic!("if statement survives folding");
        };
        assert_eq!(
            condition.kind,
            TypedExpressionKind::Literal(IrLiteral::Bool(false))
        );
        assert_eq!(condition.ty, IrType::Bool, "folds preserve logical types");
        let TypedStatementIr::Let {
            value: Some(value), ..
        } = &then_block.statements[0]
        else {
            panic!("let statement survives folding");
        };
        assert_eq!(value.kind, TypedExpressionKind::Literal(IrLiteral::I64(6)));
    }

    #[test]
    fn pass_invariant_rejects_invalid_input_before_rewrite() {
        let mut function = TypedFunctionIr {
            parameters: Vec::new(),
            locals: Vec::new(),
            return_type: IrType::Unit,
            effect: IrEffect::Ordinary,
            body: TypedBlockIr {
                statements: vec![TypedStatementIr::While {
                    condition: *literal(IrLiteral::Bool(false), IrType::Bool),
                    body: TypedBlockIr::default(),
                    max_iterations: 0,
                }],
                tail: None,
            },
        };
        let error = PassManager::standard()
            .optimize_function_checked(&mut function, &PassInvariant::isolated())
            .expect_err("zero loop bound must fail before the first pass");
        assert_eq!(error.pass, "constant-folding");
        assert_eq!(error.phase, PassInvariantPhase::Before);
        assert_eq!(error.error, TypedIrError::ZeroLoopBound);
    }

    #[test]
    fn direct_enum_match_specializes_payload_and_folds_selected_arm() {
        let enum_definition = DefinitionId(20);
        let variant = DefinitionId(21);
        let binding = DefinitionId(22);
        let span = expr(TypedExpressionKind::Literal(IrLiteral::Unit), IrType::Unit).span;
        let matched = expr(
            TypedExpressionKind::Match {
                value: Box::new(expr(
                    TypedExpressionKind::EnumConstruct {
                        enum_definition,
                        variant_definition: variant,
                        payload: Some(literal(IrLiteral::I32(41), IrType::I32)),
                    },
                    IrType::Named(enum_definition),
                )),
                arms: vec![
                    crate::ir::TypedMatchArmIr {
                        pattern: TypedPatternIr {
                            ty: IrType::Named(enum_definition),
                            span: span.clone(),
                            kind: TypedPatternKind::Variant {
                                definition: variant,
                                payload: vec![TypedPatternIr {
                                    ty: IrType::I32,
                                    span: span.clone(),
                                    kind: TypedPatternKind::Binding(binding),
                                }],
                            },
                        },
                        value: binary(
                            BinaryOperator::Add,
                            Box::new(expr(TypedExpressionKind::Reference(binding), IrType::I32)),
                            literal(IrLiteral::I32(1), IrType::I32),
                            IrType::I32,
                        ),
                    },
                    crate::ir::TypedMatchArmIr {
                        pattern: TypedPatternIr {
                            ty: IrType::Named(enum_definition),
                            span,
                            kind: TypedPatternKind::Wildcard,
                        },
                        value: *literal(IrLiteral::I32(0), IrType::I32),
                    },
                ],
            },
            IrType::I32,
        );
        let mut function = function_returning(matched);
        function.locals.push(binding);
        let reports = PassManager::standard().optimize_function(&mut function);
        assert_eq!(reports.len(), 7, "complete local Stage-D pipeline");
        assert_eq!(
            reports
                .iter()
                .find(|report| report.pass == "match-specialization")
                .map(|report| report.rewrites),
            Some(2),
            "one match plus one payload binding are removed"
        );
        assert_eq!(
            folded_return(&function),
            &TypedExpressionKind::Literal(IrLiteral::I32(42))
        );
    }

    #[test]
    fn package_constants_and_struct_fields_propagate_before_lowering() {
        let constant = DefinitionId(30);
        let structure = DefinitionId(31);
        let field = DefinitionId(32);
        let constant_value = expr(
            TypedExpressionKind::Construct {
                definition: structure,
                fields: vec![(field, *literal(IrLiteral::I64(17), IrType::I64))],
            },
            IrType::Named(structure),
        );
        let invariant = PassInvariant {
            definition_limit: usize::MAX,
            constants: Arc::new(BTreeMap::from([(constant, constant_value)])),
            aggregate_definitions: Arc::new(BTreeSet::from([structure])),
        };
        let mut function = function_returning(expr(
            TypedExpressionKind::Field {
                base: Box::new(expr(
                    TypedExpressionKind::Reference(constant),
                    IrType::Named(structure),
                )),
                field,
            },
            IrType::I64,
        ));
        PassManager::standard()
            .optimize_function_checked(&mut function, &invariant)
            .expect("package-aware constants preserve invariants");
        assert_eq!(
            folded_return(&function),
            &TypedExpressionKind::Literal(IrLiteral::I64(17))
        );
    }

    #[test]
    fn bounded_small_function_inlining_uses_real_call_counts() {
        let helper_definition = DefinitionId(40);
        let lhs = DefinitionId(41);
        let rhs = DefinitionId(42);
        let entry_definition = DefinitionId(43);
        let callee_body = binary(
            BinaryOperator::Add,
            Box::new(expr(TypedExpressionKind::Reference(lhs), IrType::I32)),
            Box::new(expr(TypedExpressionKind::Reference(rhs), IrType::I32)),
            IrType::I32,
        );
        let helper_function = TypedFunctionIr {
            parameters: vec![lhs, rhs],
            locals: Vec::new(),
            return_type: IrType::I32,
            effect: IrEffect::Ordinary,
            body: TypedBlockIr {
                statements: vec![TypedStatementIr::Return(Some(callee_body))],
                tail: None,
            },
        };
        let call = |left, right| {
            expr(
                TypedExpressionKind::Call {
                    callee: helper_definition,
                    arguments: vec![
                        *literal(IrLiteral::I32(left), IrType::I32),
                        *literal(IrLiteral::I32(right), IrType::I32),
                    ],
                },
                IrType::I32,
            )
        };
        let entry_function = function_returning(binary(
            BinaryOperator::Add,
            Box::new(call(1, 2)),
            Box::new(call(3, 4)),
            IrType::I32,
        ));
        let mut functions = BTreeMap::from([
            (helper_definition, helper_function),
            (entry_definition, entry_function),
        ]);
        let report = PassManager::standard()
            .optimize_functions(&mut functions, &PassInvariant::isolated())
            .expect("bounded inlining preserves Typed IR");
        assert_eq!(report.inlining.rewrites, 2);
        assert_eq!(
            folded_return(&functions[&entry_definition]),
            &TypedExpressionKind::Literal(IrLiteral::I32(10))
        );
    }

    #[test]
    fn materialization_report_classifies_real_aggregate_boundaries() {
        let aggregate = DefinitionId(50);
        let variant = DefinitionId(51);
        let contract = DefinitionId(52);
        let host_function = DefinitionId(53);
        let array = DefinitionId(54);
        let state = DefinitionId(55);
        let make_value = || {
            expr(
                TypedExpressionKind::EnumConstruct {
                    enum_definition: aggregate,
                    variant_definition: variant,
                    payload: None,
                },
                IrType::Named(aggregate),
            )
        };
        let function = TypedFunctionIr {
            parameters: vec![array, state],
            locals: Vec::new(),
            return_type: IrType::Unit,
            effect: IrEffect::Task,
            body: TypedBlockIr {
                statements: vec![
                    TypedStatementIr::Expression(expr(
                        TypedExpressionKind::HostCall {
                            contract,
                            function: host_function,
                            arguments: vec![make_value()],
                        },
                        IrType::Unit,
                    )),
                    TypedStatementIr::Expression(expr(
                        TypedExpressionKind::BuiltinCall {
                            operation: BuiltinOperationIr::ArrayPush,
                            type_arguments: vec![IrType::Named(aggregate)],
                            arguments: vec![
                                expr(
                                    TypedExpressionKind::Reference(array),
                                    IrType::Array(Box::new(IrType::Named(aggregate))),
                                ),
                                make_value(),
                            ],
                        },
                        IrType::Unit,
                    )),
                    TypedStatementIr::Expression(TypedExpressionIr {
                        ty: IrType::Named(aggregate),
                        effect: IrEffect::Task,
                        span: make_value().span,
                        kind: TypedExpressionKind::Await(Box::new(TypedExpressionIr {
                            ty: IrType::Named(aggregate),
                            effect: IrEffect::Task,
                            span: make_value().span,
                            kind: TypedExpressionKind::Reference(state),
                        })),
                    }),
                    TypedStatementIr::Expression(expr(
                        TypedExpressionKind::StateField {
                            base: Box::new(expr(
                                TypedExpressionKind::Reference(state),
                                IrType::Named(aggregate),
                            )),
                            field: variant,
                        },
                        IrType::Named(aggregate),
                    )),
                ],
                tail: None,
            },
        };
        let invariant = PassInvariant {
            definition_limit: usize::MAX,
            constants: Arc::default(),
            aggregate_definitions: Arc::new(BTreeSet::from([aggregate])),
        };
        let report = analyze_aggregate_materialization(
            &BTreeMap::from([(DefinitionId(56), function)]),
            &invariant,
        );
        assert_eq!(report.local_physical_values, 2);
        assert_eq!(report.host_boundaries, 1);
        assert_eq!(report.container_boundaries, 1);
        assert_eq!(report.task_suspend_boundaries, 2);
        assert_eq!(report.persistent_state_boundaries, 1);
        assert_eq!(report.total_boundaries(), 5);
    }

    #[test]
    fn dynamic_string_work_is_not_deleted_as_pure() {
        let left = DefinitionId(60);
        let right = DefinitionId(61);
        let result = DefinitionId(62);
        let mut function = TypedFunctionIr {
            parameters: vec![left, right],
            locals: vec![result],
            return_type: IrType::Unit,
            effect: IrEffect::Ordinary,
            body: TypedBlockIr {
                statements: vec![
                    TypedStatementIr::Let {
                        definition: result,
                        mutable: false,
                        value: Some(binary(
                            BinaryOperator::Add,
                            Box::new(expr(TypedExpressionKind::Reference(left), IrType::String)),
                            Box::new(expr(TypedExpressionKind::Reference(right), IrType::String)),
                            IrType::String,
                        )),
                    },
                    TypedStatementIr::Return(Some(*literal(IrLiteral::Unit, IrType::Unit))),
                ],
                tail: None,
            },
        };
        PassManager::standard().optimize_function(&mut function);
        assert!(
            matches!(
                function.body.statements.first(),
                Some(TypedStatementIr::Let {
                    definition,
                    value: Some(TypedExpressionIr {
                        kind: TypedExpressionKind::Binary { .. },
                        ..
                    }),
                    ..
                }) if *definition == result
            ),
            "dynamic String concatenation can allocate and must remain observable"
        );
    }
}
