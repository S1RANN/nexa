use crate::{
    DeclarationVisibility, DefinitionId, DefinitionKind, IrEffect, IrType, MigrationIntrinsicIr,
    SourceKey, SourceRange, TypedBlockIr, TypedDeclarationBody, TypedExpressionIr,
    TypedExpressionKind, TypedPackageIr, TypedPlaceIr, TypedStatementIr,
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

#[derive(Clone, Debug, PartialEq)]
pub struct MethodCandidate {
    pub declaration: DefinitionId,
    pub name: String,
    pub documentation: Option<String>,
    pub type_parameters: Vec<String>,
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
    let has_receiver = analysis
        .metadata()
        .inherent_methods
        .iter()
        .any(|method| method.definition == declaration && method.has_receiver);
    let parameters = function
        .parameters
        .iter()
        .skip(usize::from(has_receiver))
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

/// Source-level inherent methods available on a concrete receiver type.
#[must_use]
pub fn methods_for_type(
    analysis: &TypedPackageIr,
    source: &SourceKey,
    ty: &IrType,
) -> Vec<MethodCandidate> {
    let mut methods = method_candidates_for_type(analysis, source, ty, true);
    let receiver = match ty {
        IrType::I32 => Some("i32"),
        IrType::I64 => Some("i64"),
        IrType::F32 => Some("f32"),
        IrType::F64 => Some("f64"),
        _ => None,
    };
    if let Some(receiver) = receiver {
        methods.extend(
            nexa_stdlib::standard_library()
                .methods()
                .filter(|method| method.receiver == receiver)
                .filter_map(|method| {
                    let definition = analysis.definitions().iter().find(|definition| {
                        definition.package_id.as_str() == nexa_stdlib::PACKAGE_ID
                            && definition.module.as_str()
                                == format!("std.{}", method.implementation_module)
                            && definition.name == method.implementation
                    })?;
                    Some(MethodCandidate {
                        declaration: definition.id,
                        name: method.name.to_owned(),
                        documentation: Some(method.contract.to_owned()),
                        type_parameters: Vec::new(),
                        parameters: method
                            .parameters
                            .iter()
                            .filter_map(|parameter| {
                                semantic_scalar_type(parameter.ty).map(|ty| InstantiatedParameter {
                                    name: parameter.name.to_owned(),
                                    ty,
                                })
                            })
                            .collect(),
                        result: semantic_scalar_type(method.result)?,
                        effect: definition.effect,
                        span: definition.span.clone(),
                    })
                }),
        );
    }
    methods
}

/// Source-level associated functions available through a concrete type namespace.
#[must_use]
pub fn associated_functions_for_type(
    analysis: &TypedPackageIr,
    source: &SourceKey,
    ty: &IrType,
) -> Vec<MethodCandidate> {
    method_candidates_for_type(analysis, source, ty, false)
}

fn method_candidates_for_type(
    analysis: &TypedPackageIr,
    source: &SourceKey,
    ty: &IrType,
    has_receiver: bool,
) -> Vec<MethodCandidate> {
    let IrType::Named(owner) = ty else {
        return Vec::new();
    };
    let origin = analysis
        .metadata()
        .generic_nominal_instances
        .iter()
        .find(|instance| instance.instance == *owner);
    let declaration = origin.map_or(*owner, |instance| instance.declaration);
    let arguments = origin.map_or(&[][..], |instance| instance.arguments.as_slice());
    analysis
        .metadata()
        .inherent_methods
        .iter()
        .filter(|method| method.owner == declaration && method.has_receiver == has_receiver)
        .filter_map(|method| {
            let definition = analysis.definition(method.definition)?;
            let current = analysis
                .modules()
                .iter()
                .find(|module| &module.source == source)?;
            let visible = if definition.package_id == current.package_id
                && definition.module == current.module
            {
                true
            } else if definition.package_id == current.package_id {
                definition.visibility != DeclarationVisibility::Private
            } else {
                definition.visibility == DeclarationVisibility::Public
            };
            if !visible {
                return None;
            }
            Some(MethodCandidate {
                declaration: method.definition,
                name: method.name.clone(),
                documentation: method.documentation.clone(),
                type_parameters: method
                    .type_parameters
                    .iter()
                    .skip(method.impl_type_parameter_count)
                    .cloned()
                    .collect(),
                parameters: method
                    .parameters
                    .iter()
                    .skip(usize::from(has_receiver))
                    .map(|(name, ty)| InstantiatedParameter {
                        name: name.clone(),
                        ty: substitute_semantic_type(ty, arguments),
                    })
                    .collect(),
                result: substitute_semantic_type(&method.result, arguments),
                effect: method.effect,
                span: definition.span.clone(),
            })
        })
        .collect()
}

fn semantic_scalar_type(name: &str) -> Option<IrType> {
    match name {
        "i32" => Some(IrType::I32),
        "i64" => Some(IrType::I64),
        "f32" => Some(IrType::F32),
        "f64" => Some(IrType::F64),
        "string" => Some(IrType::String),
        _ => None,
    }
}

fn substitute_semantic_type(ty: &IrType, arguments: &[IrType]) -> IrType {
    match ty {
        IrType::TypeParameter(index) => {
            arguments
                .get(usize::from(*index))
                .cloned()
                .unwrap_or_else(|| {
                    IrType::TypeParameter(
                        index.saturating_sub(u16::try_from(arguments.len()).unwrap_or(u16::MAX)),
                    )
                })
        }
        IrType::Option(inner) => {
            IrType::Option(Box::new(substitute_semantic_type(inner, arguments)))
        }
        IrType::Result(ok, error) => IrType::Result(
            Box::new(substitute_semantic_type(ok, arguments)),
            Box::new(substitute_semantic_type(error, arguments)),
        ),
        IrType::Array(inner) => IrType::Array(Box::new(substitute_semantic_type(inner, arguments))),
        IrType::Map(key, value) => IrType::Map(
            Box::new(substitute_semantic_type(key, arguments)),
            Box::new(substitute_semantic_type(value, arguments)),
        ),
        IrType::Set(inner) => IrType::Set(Box::new(substitute_semantic_type(inner, arguments))),
        IrType::Tuple(items) => IrType::Tuple(
            items
                .iter()
                .map(|item| substitute_semantic_type(item, arguments))
                .collect(),
        ),
        IrType::HostRequest(inner) => IrType::HostRequest(
            inner
                .as_ref()
                .map(|inner| Box::new(substitute_semantic_type(inner, arguments))),
        ),
        IrType::ResourceToken(inner) => IrType::ResourceToken(
            inner
                .as_ref()
                .map(|inner| Box::new(substitute_semantic_type(inner, arguments))),
        ),
        IrType::Snapshot(inner) => {
            IrType::Snapshot(Box::new(substitute_semantic_type(inner, arguments)))
        }
        IrType::Buffer(inner) => {
            IrType::Buffer(Box::new(substitute_semantic_type(inner, arguments)))
        }
        IrType::StateHandle(inner) => {
            IrType::StateHandle(Box::new(substitute_semantic_type(inner, arguments)))
        }
        IrType::Error
        | IrType::Unit
        | IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune
        | IrType::Named(_) => ty.clone(),
    }
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
        IrType::Named(definition) => analysis
            .metadata()
            .generic_nominal_instances
            .iter()
            .find(|instance| instance.instance == *definition)
            .and_then(|instance| {
                analysis
                    .definition(instance.declaration)
                    .map(|declaration| {
                        format!(
                            "{}<{}>",
                            declaration.name,
                            instance
                                .arguments
                                .iter()
                                .map(|argument| display_type(argument, analysis))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
            })
            .or_else(|| {
                analysis
                    .definition(*definition)
                    .map(|definition| definition.name.clone())
            })
            .unwrap_or_else(|| format!("type#{}", definition.0)),
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
