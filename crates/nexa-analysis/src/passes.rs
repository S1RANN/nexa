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

use crate::ir::{
    BinaryOperator, IrLiteral, MigrationIntrinsicIr, TypedBlockIr, TypedExpressionIr,
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
    #[must_use]
    pub fn standard() -> Self {
        Self {
            passes: vec![Box::new(ConstantFolding)],
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
        assert_eq!(reports.len(), 1);
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
        let reports = PassManager::standard().optimize_function(&mut function);
        assert_eq!(reports[0].rewrites, 2);
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
