use crate::{
    DefinitionId, DefinitionKind, IrEffect, IrType, MigrationIntrinsicIr, SourceKey, SourceRange,
    TypedBlockIr, TypedDeclarationBody, TypedExpressionIr, TypedExpressionKind, TypedPackageIr,
    TypedPlaceIr, TypedStatementIr,
};

#[derive(Clone, Debug, PartialEq)]
pub struct InstantiatedParameter {
    pub name: String,
    pub ty: IrType,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstantiatedSignature {
    pub declaration: DefinitionId,
    pub instance: DefinitionId,
    pub name: String,
    pub parameters: Vec<InstantiatedParameter>,
    pub result: IrType,
    pub effect: IrEffect,
    pub span: SourceRange,
}

#[must_use]
pub fn definition_at(
    analysis: &TypedPackageIr,
    source: &SourceKey,
    offset: u32,
) -> Option<DefinitionId> {
    let reference = analysis
        .modules()
        .iter()
        .filter(|module| &module.source == source)
        .flat_map(|module| module.resolved_references.iter())
        .filter(|reference| contains_offset(&reference.span, offset))
        .min_by_key(|reference| reference.span.end.saturating_sub(reference.span.start));
    if let Some(reference) = reference {
        return Some(reference.target);
    }
    analysis
        .definitions()
        .iter()
        .filter(|definition| &definition.span.source == source)
        .filter(|definition| contains_offset(&definition.span, offset))
        .min_by_key(|definition| definition.span.end.saturating_sub(definition.span.start))
        .map(|definition| definition.id)
}

/// Smallest source-visible reference/declaration span containing `offset`.
#[must_use]
pub fn semantic_span_at(
    analysis: &TypedPackageIr,
    source: &SourceKey,
    offset: u32,
) -> Option<SourceRange> {
    analysis
        .modules()
        .iter()
        .filter(|module| &module.source == source)
        .flat_map(|module| {
            module
                .resolved_references
                .iter()
                .map(|reference| &reference.span)
        })
        .chain(
            analysis
                .definitions()
                .iter()
                .map(|definition| &definition.span),
        )
        .filter(|span| &span.source == source && contains_offset(span, offset))
        .min_by_key(|span| span.end.saturating_sub(span.start))
        .cloned()
}

#[must_use]
pub fn type_at(analysis: &TypedPackageIr, source: &SourceKey, offset: u32) -> Option<IrType> {
    let mut best = None::<(&IrType, u32)>;
    for module in analysis
        .modules()
        .iter()
        .filter(|module| &module.source == source)
    {
        for declaration in module.declarations.iter() {
            visit_declaration_expressions(&declaration.body, &mut |expression| {
                if contains_offset(&expression.span, offset) {
                    let width = expression.span.end.saturating_sub(expression.span.start);
                    if best.is_none_or(|(_, best_width)| width <= best_width) {
                        best = Some((&expression.ty, width));
                    }
                }
            });
        }
    }
    best.map(|(ty, _)| ty.clone()).or_else(|| {
        definition_at(analysis, source, offset)
            .and_then(|definition| analysis.definition(definition))
            .map(|definition| definition.ty.clone())
    })
}

#[must_use]
pub fn call_signature_at(
    analysis: &TypedPackageIr,
    source: &SourceKey,
    offset: u32,
) -> Option<InstantiatedSignature> {
    let mut best = None::<(DefinitionId, &SourceRange, u32)>;
    for module in analysis
        .modules()
        .iter()
        .filter(|module| &module.source == source)
    {
        for declaration in module.declarations.iter() {
            visit_declaration_expressions(&declaration.body, &mut |expression| {
                let TypedExpressionKind::Call { callee, .. } = expression.kind else {
                    return;
                };
                if !contains_offset(&expression.span, offset) {
                    return;
                }
                let width = expression.span.end.saturating_sub(expression.span.start);
                if best.is_none_or(|(_, _, best_width)| width <= best_width) {
                    best = Some((callee, &expression.span, width));
                }
            });
        }
    }
    let (instance, span, _) = best?;
    let definition = analysis.definition(instance)?;
    let function = analysis
        .modules()
        .iter()
        .flat_map(|module| module.declarations.iter())
        .find_map(|declaration| {
            (declaration.definition == instance)
                .then_some(&declaration.body)
                .and_then(|body| match body {
                    TypedDeclarationBody::Function(function) => Some(function),
                    TypedDeclarationBody::Const(_)
                    | TypedDeclarationBody::TypeLayout(_)
                    | TypedDeclarationBody::External => None,
                })
        })?;
    let (source_name, display_name) = definition.name.split_once("$instance$").map_or_else(
        || (definition.name.as_str(), definition.name.clone()),
        |(name, arguments)| (name, format!("{name}{arguments}")),
    );
    let declaration = analysis
        .definitions()
        .iter()
        .find(|candidate| {
            candidate.package_id == definition.package_id
                && candidate.module == definition.module
                && candidate.name == source_name
                && matches!(
                    candidate.kind,
                    DefinitionKind::Function | DefinitionKind::Task
                )
        })
        .map_or(instance, |candidate| candidate.id);
    let parameters = function
        .parameters
        .iter()
        .filter_map(|parameter| analysis.definition(*parameter))
        .map(|parameter| InstantiatedParameter {
            name: parameter.name.clone(),
            ty: parameter.ty.clone(),
        })
        .collect();
    Some(InstantiatedSignature {
        declaration,
        instance,
        name: display_name,
        parameters,
        result: function.return_type.clone(),
        effect: function.effect,
        span: span.clone(),
    })
}

#[must_use]
pub fn display_type(ty: &IrType, analysis: &TypedPackageIr) -> String {
    match ty {
        IrType::Error => "<error>".into(),
        IrType::Unit => "unit".into(),
        IrType::Bool => "bool".into(),
        IrType::I32 => "i32".into(),
        IrType::I64 => "i64".into(),
        IrType::F32 => "f32".into(),
        IrType::F64 => "f64".into(),
        IrType::String => "string".into(),
        IrType::Rune => "rune".into(),
        IrType::Named(definition) => analysis.definition(*definition).map_or_else(
            || format!("type#{}", definition.0),
            |definition| definition.name.clone(),
        ),
        IrType::Option(inner) => format!("Option<{}>", display_type(inner, analysis)),
        IrType::Result(ok, error) => format!(
            "Result<{}, {}>",
            display_type(ok, analysis),
            display_type(error, analysis)
        ),
        IrType::Array(inner) => format!("Array<{}>", display_type(inner, analysis)),
        IrType::Map(key, value) => format!(
            "Map<{}, {}>",
            display_type(key, analysis),
            display_type(value, analysis)
        ),
        IrType::Set(inner) => format!("Set<{}>", display_type(inner, analysis)),
        IrType::Tuple(values) => format!(
            "({})",
            values
                .iter()
                .map(|value| display_type(value, analysis))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        IrType::HostRequest(None) => "HostRequest".into(),
        IrType::HostRequest(Some(inner)) => {
            format!("HostRequest<{}>", display_type(inner, analysis))
        }
        IrType::ResourceToken(None) => "Token".into(),
        IrType::ResourceToken(Some(inner)) => {
            format!("Token<{}>", display_type(inner, analysis))
        }
        IrType::Snapshot(inner) => format!("Snapshot<{}>", display_type(inner, analysis)),
        IrType::Buffer(inner) => format!("Buffer<{}>", display_type(inner, analysis)),
        IrType::StateHandle(inner) => {
            format!("StateHandle<{}>", display_type(inner, analysis))
        }
        IrType::TypeParameter(index) => format!("T{index}"),
    }
}

const fn contains_offset(span: &SourceRange, offset: u32) -> bool {
    span.start <= offset && (offset < span.end || span.start == span.end && offset == span.start)
}

fn visit_declaration_expressions<'a>(
    declaration: &'a TypedDeclarationBody,
    visitor: &mut impl FnMut(&'a TypedExpressionIr),
) {
    match declaration {
        TypedDeclarationBody::Function(function) => visit_block(&function.body, visitor),
        TypedDeclarationBody::Const(expression) => visit_expression(expression, visitor),
        TypedDeclarationBody::TypeLayout(_) | TypedDeclarationBody::External => {}
    }
}

fn visit_block<'a>(block: &'a TypedBlockIr, visitor: &mut impl FnMut(&'a TypedExpressionIr)) {
    for statement in &block.statements {
        visit_statement(statement, visitor);
    }
    if let Some(tail) = block.tail.as_deref() {
        visit_expression(tail, visitor);
    }
}

#[allow(clippy::too_many_lines)]
fn visit_statement<'a>(
    statement: &'a TypedStatementIr,
    visitor: &mut impl FnMut(&'a TypedExpressionIr),
) {
    match statement {
        TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
            if let Some(value) = value {
                visit_expression(value, visitor);
            }
        }
        TypedStatementIr::Assign { target, value }
        | TypedStatementIr::CompoundAssign { target, value, .. } => {
            visit_place(target, visitor);
            visit_expression(value, visitor);
        }
        TypedStatementIr::Expression(expression) => visit_expression(expression, visitor),
        TypedStatementIr::If {
            condition,
            then_block,
            else_block,
        } => {
            visit_expression(condition, visitor);
            visit_block(then_block, visitor);
            if let Some(else_block) = else_block {
                visit_block(else_block, visitor);
            }
        }
        TypedStatementIr::While {
            condition, body, ..
        } => {
            visit_expression(condition, visitor);
            visit_block(body, visitor);
        }
        TypedStatementIr::StaticRangeFor {
            start, end, body, ..
        }
        | TypedStatementIr::DynamicRangeFor {
            start, end, body, ..
        } => {
            visit_expression(start, visitor);
            visit_expression(end, visitor);
            visit_block(body, visitor);
        }
        TypedStatementIr::CollectionFor { iterable, body, .. } => {
            visit_expression(iterable, visitor);
            visit_block(body, visitor);
        }
        TypedStatementIr::Defer { captures, .. } => {
            for capture in captures {
                visit_expression(capture, visitor);
            }
        }
        TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield { .. } => {}
    }
}

fn visit_place<'a>(place: &'a TypedPlaceIr, visitor: &mut impl FnMut(&'a TypedExpressionIr)) {
    match place {
        TypedPlaceIr::Definition(_) => {}
        TypedPlaceIr::Field { base, .. } => visit_place(base, visitor),
        TypedPlaceIr::ClassField { object, .. } => visit_expression(object, visitor),
        TypedPlaceIr::Index { base, index } => {
            visit_expression(base, visitor);
            visit_expression(index, visitor);
        }
        TypedPlaceIr::StateField { base, .. } => visit_expression(base, visitor),
    }
}

#[allow(clippy::too_many_lines)]
fn visit_expression<'a>(
    expression: &'a TypedExpressionIr,
    visitor: &mut impl FnMut(&'a TypedExpressionIr),
) {
    visitor(expression);
    match &expression.kind {
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Reference(_)
        | TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::Yield => {}
        TypedExpressionKind::Unary { operand, .. }
        | TypedExpressionKind::Try(operand)
        | TypedExpressionKind::Await(operand) => visit_expression(operand, visitor),
        TypedExpressionKind::Binary { left, right, .. } => {
            visit_expression(left, visitor);
            visit_expression(right, visitor);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => {
            for argument in arguments {
                visit_expression(argument, visitor);
            }
        }
        TypedExpressionKind::Construct { fields, .. }
        | TypedExpressionKind::ClassConstruct { fields, .. }
        | TypedExpressionKind::Update { fields, .. } => {
            for (_, value) in fields {
                visit_expression(value, visitor);
            }
            if let TypedExpressionKind::ClassConstruct { update, .. } = &expression.kind
                && let Some(update) = update
            {
                visit_expression(update, visitor);
            }
            if let TypedExpressionKind::Update { base, .. } = &expression.kind {
                visit_expression(base, visitor);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                visit_expression(payload, visitor);
            }
        }
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            visit_expression(base, visitor);
        }
        TypedExpressionKind::Index { base, index } => {
            visit_expression(base, visitor);
            visit_expression(index, visitor);
        }
        TypedExpressionKind::Array(values)
        | TypedExpressionKind::Tuple(values)
        | TypedExpressionKind::StringInterpolation(values) => {
            for value in values {
                visit_expression(value, visitor);
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            visit_expression(value, visitor);
            for arm in arms {
                visit_expression(&arm.value, visitor);
            }
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. } => {
                visit_expression(object, visitor);
            }
            MigrationIntrinsicIr::NewSet { object, value, .. } => {
                visit_expression(object, visitor);
                visit_expression(value, visitor);
            }
            MigrationIntrinsicIr::Replace { target, .. } => visit_expression(target, visitor),
            MigrationIntrinsicIr::OldGet { .. }
            | MigrationIntrinsicIr::NewCreate { .. }
            | MigrationIntrinsicIr::Preserve { .. }
            | MigrationIntrinsicIr::Delete { .. }
            | MigrationIntrinsicIr::Finish => {}
        },
    }
}
