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

use crate::ir::{
    BinaryOperator, DefinitionId, IrLiteral, MigrationIntrinsicIr, TypedBlockIr, TypedExpressionIr,
    TypedExpressionKind, TypedFunctionIr, TypedPlaceIr, TypedStatementIr, UnaryOperator,
};

/// One Typed IR rewrite pass (WP37).
pub trait TypedIrPass {
    fn name(&self) -> &'static str;
    fn run_function(&self, function: &mut TypedFunctionIr, context: &mut PassContext);
}

/// Mutable bookkeeping shared with a running pass.
#[derive(Debug, Default)]
pub struct PassContext {
    pub rewrites: u64,
}

/// Evidence record for one executed pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassReport {
    pub pass: &'static str,
    pub rewrites: u64,
}

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
                Box::new(DeadCodeElimination),
            ],
        }
    }

    /// Runs every pass over one function, returning per-pass evidence.
    pub fn optimize_function(&self, function: &mut TypedFunctionIr) -> Vec<PassReport> {
        let mut reports = Vec::with_capacity(self.passes.len());
        for pass in &self.passes {
            let mut context = PassContext::default();
            pass.run_function(function, &mut context);
            reports.push(PassReport {
                pass: pass.name(),
                rewrites: context.rewrites,
            });
        }
        reports
    }
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

/// WP39: substitutes references to immutable scalar-literal bindings.
///
/// String literals are intentionally not propagated: duplicating them would
/// multiply `LoadString` allocations until the module constant pool lands
/// (WP56). The binding itself stays in place for WP41 to remove once it is
/// provably unused.
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
                        (PropagationMode::Constants, TypedExpressionKind::Literal(literal))
                            if propagatable_literal(literal) =>
                        {
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
            _ => return,
        }
    }
    for_each_child_expression(expression, &mut |child| {
        substitute_expression(child, scopes, mode, context);
    });
}

const fn propagatable_literal(literal: &IrLiteral) -> bool {
    !matches!(literal, IrLiteral::String(_))
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
    if let Some(folded) = folded_literal(expression) {
        expression.kind = TypedExpressionKind::Literal(folded);
        context.rewrites = context.rewrites.saturating_add(1);
    }
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
            !matches!(operator, BinaryOperator::Divide | BinaryOperator::Remainder)
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
        // String concatenation allocates on the VM heap and string equality
        // is runtime content comparison; both stay observable runtime work
        // until the string constant pool lands (WP56).
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
        assert_eq!(reports.len(), 5, "standard pipeline runs five passes");
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
    fn mutable_bindings_and_strings_are_never_propagated() {
        // let mut m = 1; m = 2; let s = "x"; return m; (s unused but string)
        let mut function = TypedFunctionIr {
            parameters: Vec::new(),
            locals: vec![DefinitionId(4), DefinitionId(5)],
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
                    TypedStatementIr::Let {
                        definition: DefinitionId(5),
                        mutable: false,
                        value: Some(*literal(IrLiteral::String("x".into()), IrType::String)),
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
}
