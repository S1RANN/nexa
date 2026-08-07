fn binary_result(
    operator: BinaryOperatorKind,
    operand: &IrType,
    definitions: &[Definition],
    type_metadata: &BTreeMap<DefinitionId, TypeMetadata>,
    variant_payloads: &BTreeMap<DefinitionId, Vec<IrType>>,
    host_types: &[AnalyzedHostType],
) -> Option<IrType> {
    match operator {
        BinaryOperatorKind::Equal | BinaryOperatorKind::NotEqual
            if equality_supported(
                operand,
                definitions,
                type_metadata,
                variant_payloads,
                host_types,
            ) =>
        {
            Some(IrType::Bool)
        }
        BinaryOperatorKind::Less
        | BinaryOperatorKind::LessEqual
        | BinaryOperatorKind::Greater
        | BinaryOperatorKind::GreaterEqual
            if is_numeric(operand) =>
        {
            Some(IrType::Bool)
        }
        BinaryOperatorKind::And | BinaryOperatorKind::Or if operand == &IrType::Bool => {
            Some(IrType::Bool)
        }
        BinaryOperatorKind::Add if operand == &IrType::String => Some(IrType::String),
        BinaryOperatorKind::Add
        | BinaryOperatorKind::Subtract
        | BinaryOperatorKind::Multiply
        | BinaryOperatorKind::Divide
        | BinaryOperatorKind::Remainder
            if is_numeric(operand) =>
        {
            Some(operand.clone())
        }
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
fn equality_supported(
    operand: &IrType,
    definitions: &[Definition],
    type_metadata: &BTreeMap<DefinitionId, TypeMetadata>,
    variant_payloads: &BTreeMap<DefinitionId, Vec<IrType>>,
    host_types: &[AnalyzedHostType],
) -> bool {
    #[allow(clippy::too_many_lines)]
    fn visit(
        operand: &IrType,
        definitions: &[Definition],
        type_metadata: &BTreeMap<DefinitionId, TypeMetadata>,
        variant_payloads: &BTreeMap<DefinitionId, Vec<IrType>>,
        host_types: &[AnalyzedHostType],
        visiting: &mut BTreeSet<DefinitionId>,
    ) -> bool {
        match operand {
            IrType::Unit
            | IrType::Bool
            | IrType::I32
            | IrType::I64
            | IrType::F32
            | IrType::F64
            | IrType::String
            | IrType::Rune
            | IrType::Error => true,
            IrType::Option(inner) => visit(
                inner,
                definitions,
                type_metadata,
                variant_payloads,
                host_types,
                visiting,
            ),
            IrType::Result(ok, error) => {
                visit(
                    ok,
                    definitions,
                    type_metadata,
                    variant_payloads,
                    host_types,
                    visiting,
                ) && visit(
                    error,
                    definitions,
                    type_metadata,
                    variant_payloads,
                    host_types,
                    visiting,
                )
            }
            IrType::Tuple(values) => values.iter().all(|value| {
                visit(
                    value,
                    definitions,
                    type_metadata,
                    variant_payloads,
                    host_types,
                    visiting,
                )
            }),
            IrType::Named(definition) => {
                let Some(declaration) = definitions.get(definition.0 as usize) else {
                    return false;
                };
                if declaration.kind == DefinitionKind::Class {
                    // Class equality is object identity and never traverses fields.
                    return true;
                }
                if !visiting.insert(*definition) {
                    // Recursive inline layouts are diagnosed independently. This guard keeps type
                    // recovery total without inventing resource equality.
                    return true;
                }
                let supported = match declaration.kind {
                    DefinitionKind::Struct => {
                        type_metadata.get(definition).is_some_and(|metadata| {
                            metadata.field_order.iter().all(|field| {
                                definitions.get(field.0 as usize).is_some_and(|field| {
                                    visit(
                                        &field.ty,
                                        definitions,
                                        type_metadata,
                                        variant_payloads,
                                        host_types,
                                        visiting,
                                    )
                                })
                            })
                        })
                    }
                    DefinitionKind::Enum => type_metadata.get(definition).is_some_and(|metadata| {
                        metadata.variant_order.iter().all(|variant| {
                            variant_payloads.get(variant).is_none_or(|payload| {
                                payload.iter().all(|value| {
                                    visit(
                                        value,
                                        definitions,
                                        type_metadata,
                                        variant_payloads,
                                        host_types,
                                        visiting,
                                    )
                                })
                            })
                        })
                    }),
                    DefinitionKind::HostContract => host_types
                        .iter()
                        .find(|host_type| host_type.definition == *definition)
                        .is_some_and(|host_type| match host_type.kind {
                            ExternalTypeKind::Opaque => false,
                            ExternalTypeKind::Struct => {
                                host_type.fields.iter().all(|(field, _)| {
                                    definitions.get(field.0 as usize).is_some_and(|field| {
                                        visit(
                                            &field.ty,
                                            definitions,
                                            type_metadata,
                                            variant_payloads,
                                            host_types,
                                            visiting,
                                        )
                                    })
                                })
                            }
                            ExternalTypeKind::Enum => {
                                host_type.variants.iter().all(|(variant, _)| {
                                    variant_payloads.get(variant).is_none_or(|payload| {
                                        payload.iter().all(|value| {
                                            visit(
                                                value,
                                                definitions,
                                                type_metadata,
                                                variant_payloads,
                                                host_types,
                                                visiting,
                                            )
                                        })
                                    })
                                })
                            }
                        }),
                    DefinitionKind::Function
                    | DefinitionKind::Task
                    | DefinitionKind::Class
                    | DefinitionKind::Const
                    | DefinitionKind::Field
                    | DefinitionKind::Variant
                    | DefinitionKind::Parameter
                    | DefinitionKind::Local
                    | DefinitionKind::HostFunction
                    | DefinitionKind::StandardLibrary => false,
                };
                visiting.remove(definition);
                supported
            }
            IrType::Array(_)
            | IrType::Map(_, _)
            | IrType::Set(_)
            | IrType::HostRequest(_)
            | IrType::ResourceToken(_)
            | IrType::Snapshot(_)
            | IrType::Buffer(_)
            | IrType::StateHandle(_)
            | IrType::TypeParameter(_) => false,
        }
    }

    visit(
        operand,
        definitions,
        type_metadata,
        variant_payloads,
        host_types,
        &mut BTreeSet::new(),
    )
}

fn const_safe_type(
    ty: &IrType,
    definitions: &[Definition],
    type_metadata: &BTreeMap<DefinitionId, TypeMetadata>,
    variant_payloads: &BTreeMap<DefinitionId, Vec<IrType>>,
    visiting: &mut BTreeSet<DefinitionId>,
) -> bool {
    match ty {
        IrType::Unit
        | IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune
        | IrType::Error => true,
        IrType::Option(inner) => const_safe_type(
            inner,
            definitions,
            type_metadata,
            variant_payloads,
            visiting,
        ),
        IrType::Result(ok, error) => {
            const_safe_type(ok, definitions, type_metadata, variant_payloads, visiting)
                && const_safe_type(
                    error,
                    definitions,
                    type_metadata,
                    variant_payloads,
                    visiting,
                )
        }
        IrType::Tuple(values) => values.iter().all(|value| {
            const_safe_type(
                value,
                definitions,
                type_metadata,
                variant_payloads,
                visiting,
            )
        }),
        IrType::Named(definition) => {
            let Some(declaration) = definitions.get(definition.0 as usize) else {
                return false;
            };
            if !visiting.insert(*definition) {
                return false;
            }
            let safe = match declaration.kind {
                DefinitionKind::Struct => type_metadata.get(definition).is_some_and(|metadata| {
                    metadata.field_order.iter().all(|field| {
                        definitions.get(field.0 as usize).is_some_and(|field| {
                            const_safe_type(
                                &field.ty,
                                definitions,
                                type_metadata,
                                variant_payloads,
                                visiting,
                            )
                        })
                    })
                }),
                DefinitionKind::Enum => type_metadata.get(definition).is_some_and(|metadata| {
                    metadata.variant_order.iter().all(|variant| {
                        variant_payloads.get(variant).is_none_or(|payload| {
                            payload.iter().all(|value| {
                                const_safe_type(
                                    value,
                                    definitions,
                                    type_metadata,
                                    variant_payloads,
                                    visiting,
                                )
                            })
                        })
                    })
                }),
                DefinitionKind::Function
                | DefinitionKind::Task
                | DefinitionKind::Class
                | DefinitionKind::Const
                | DefinitionKind::Field
                | DefinitionKind::Variant
                | DefinitionKind::Parameter
                | DefinitionKind::Local
                | DefinitionKind::HostContract
                | DefinitionKind::HostFunction
                | DefinitionKind::StandardLibrary => false,
            };
            visiting.remove(definition);
            safe
        }
        IrType::Array(_)
        | IrType::Map(_, _)
        | IrType::Set(_)
        | IrType::HostRequest(_)
        | IrType::ResourceToken(_)
        | IrType::Snapshot(_)
        | IrType::Buffer(_)
        | IrType::StateHandle(_)
        | IrType::TypeParameter(_) => false,
    }
}

fn constant_i32_expression(
    expression: &TypedExpressionIr,
    constants: &BTreeMap<DefinitionId, ConstValue>,
) -> Option<i32> {
    match &expression.kind {
        TypedExpressionKind::Literal(IrLiteral::I32(value)) => Some(*value),
        TypedExpressionKind::Reference(definition) => match constants.get(definition) {
            Some(ConstValue::I32(value)) => Some(*value),
            _ => None,
        },
        TypedExpressionKind::Unary {
            operator: UnaryOperator::Negate,
            operand,
        } => constant_i32_expression(operand, constants)?.checked_neg(),
        TypedExpressionKind::Binary {
            operator,
            left,
            right,
        } => {
            let left = constant_i32_expression(left, constants)?;
            let right = constant_i32_expression(right, constants)?;
            match operator {
                BinaryOperator::Add => left.checked_add(right),
                BinaryOperator::Subtract => left.checked_sub(right),
                BinaryOperator::Multiply => left.checked_mul(right),
                BinaryOperator::Divide => left.checked_div(right),
                BinaryOperator::Remainder => left.checked_rem(right),
                BinaryOperator::Equal
                | BinaryOperator::NotEqual
                | BinaryOperator::Less
                | BinaryOperator::LessEqual
                | BinaryOperator::Greater
                | BinaryOperator::GreaterEqual
                | BinaryOperator::And
                | BinaryOperator::Or => None,
            }
        }
        _ => None,
    }
}

fn constant_bool_expression(
    expression: &TypedExpressionIr,
    constants: &BTreeMap<DefinitionId, ConstValue>,
) -> Option<bool> {
    match &expression.kind {
        TypedExpressionKind::Literal(IrLiteral::Bool(value)) => Some(*value),
        TypedExpressionKind::Reference(definition) => match constants.get(definition) {
            Some(ConstValue::Bool(value)) => Some(*value),
            _ => None,
        },
        TypedExpressionKind::Unary {
            operator: UnaryOperator::Not,
            operand,
        } => constant_bool_expression(operand, constants).map(|value| !value),
        TypedExpressionKind::Binary {
            operator: BinaryOperator::And,
            left,
            right,
        } => Some(
            constant_bool_expression(left, constants)?
                && constant_bool_expression(right, constants)?,
        ),
        TypedExpressionKind::Binary {
            operator: BinaryOperator::Or,
            left,
            right,
        } => Some(
            constant_bool_expression(left, constants)?
                || constant_bool_expression(right, constants)?,
        ),
        TypedExpressionKind::Binary {
            operator: BinaryOperator::Equal,
            left,
            right,
        } => match (
            constant_bool_expression(left, constants),
            constant_bool_expression(right, constants),
        ) {
            (Some(left), Some(right)) => Some(left == right),
            _ => Some(
                constant_i32_expression(left, constants)?
                    == constant_i32_expression(right, constants)?,
            ),
        },
        TypedExpressionKind::Binary {
            operator: BinaryOperator::NotEqual,
            left,
            right,
        } => match (
            constant_bool_expression(left, constants),
            constant_bool_expression(right, constants),
        ) {
            (Some(left), Some(right)) => Some(left != right),
            _ => Some(
                constant_i32_expression(left, constants)?
                    != constant_i32_expression(right, constants)?,
            ),
        },
        TypedExpressionKind::Binary {
            operator:
                operator @ (BinaryOperator::Less
                | BinaryOperator::LessEqual
                | BinaryOperator::Greater
                | BinaryOperator::GreaterEqual),
            left,
            right,
        } => {
            let left = constant_i32_expression(left, constants)?;
            let right = constant_i32_expression(right, constants)?;
            Some(match operator {
                BinaryOperator::Less => left < right,
                BinaryOperator::LessEqual => left <= right,
                BinaryOperator::Greater => left > right,
                BinaryOperator::GreaterEqual => left >= right,
                _ => unreachable!("comparison pattern is exhaustive"),
            })
        }
        _ => None,
    }
}

fn map_migration_paths(
    paths: &mut MigrationPaths,
    mut update: impl FnMut(&mut MigrationPathState),
) {
    let mut updated = MigrationPaths::new();
    for mut path in std::mem::take(paths) {
        update(&mut path);
        updated.insert(path);
    }
    *paths = updated;
}

fn record_migration_operation(paths: &mut MigrationPaths, span: &SourceRange) {
    map_migration_paths(paths, |path| {
        if path.finish_count >= 1 {
            path.operation_after_finish
                .get_or_insert((span.start, span.end));
        }
    });
}

fn record_migration_forwarding(paths: &mut MigrationPaths, identity: StableId, span: &SourceRange) {
    map_migration_paths(paths, |path| {
        let count = path.forwarding.entry(identity).or_default();
        if *count >= 1 {
            path.duplicate_forwarding
                .entry(identity)
                .or_insert((span.start, span.end));
        }
        *count = count.saturating_add(1).min(2);
    });
}

fn record_migration_finish(paths: &mut MigrationPaths, span: &SourceRange) {
    map_migration_paths(paths, |path| {
        let unforwarded = path
            .reads
            .iter()
            .filter(|identity| path.forwarding.get(identity).copied().unwrap_or_default() == 0)
            .copied()
            .collect::<Vec<_>>();
        path.unforwarded_at_finish.extend(unforwarded);
        if path.finish_count >= 1 {
            path.duplicate_finish.get_or_insert((span.start, span.end));
        }
        path.finish_count = path.finish_count.saturating_add(1).min(2);
    });
}

fn is_numeric(ty: &IrType) -> bool {
    matches!(ty, IrType::I32 | IrType::I64 | IrType::F32 | IrType::F64)
}

fn is_scalar(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::Bool
            | IrType::I32
            | IrType::I64
            | IrType::F32
            | IrType::F64
            | IrType::String
            | IrType::Rune
    )
}

/// Runtime-formattable values handled by the generic collection formatter.
/// Nominal values use a compiler-emitted field plan so their declared names
/// remain available without adding reflection metadata to Bytecode v8.
fn is_runtime_interpolatable(ty: &IrType) -> bool {
    match ty {
        IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune => true,
        IrType::Array(inner) => is_runtime_interpolatable(inner),
        _ => false,
    }
}

/// Recursively formattable interpolation set: scalars, scalar Arrays, and
/// acyclic Struct/Class field graphs. Nominal cycles and fields whose values
/// lack deterministic formatting remain compile-time errors.
fn is_interpolatable(
    ty: &IrType,
    definitions: &[Definition],
    type_metadata: &BTreeMap<DefinitionId, TypeMetadata>,
) -> bool {
    fn visit(
        ty: &IrType,
        definitions: &[Definition],
        type_metadata: &BTreeMap<DefinitionId, TypeMetadata>,
        visiting: &mut BTreeSet<DefinitionId>,
    ) -> bool {
        if is_runtime_interpolatable(ty) {
            return true;
        }
        let IrType::Named(definition) = ty else {
            return false;
        };
        let Some(declaration) = definitions.get(definition.0 as usize) else {
            return false;
        };
        if !matches!(
            declaration.kind,
            DefinitionKind::Struct | DefinitionKind::Class
        ) || !visiting.insert(*definition)
        {
            return false;
        }
        let formattable = type_metadata.get(definition).is_some_and(|metadata| {
            metadata.field_order.iter().all(|field| {
                definitions.get(field.0 as usize).is_some_and(|field| {
                    visit(&field.ty, definitions, type_metadata, visiting)
                })
            })
        });
        visiting.remove(definition);
        formattable
    }

    visit(ty, definitions, type_metadata, &mut BTreeSet::new())
}

fn max_effect(left: IrEffect, right: IrEffect) -> IrEffect {
    if left == IrEffect::Task || right == IrEffect::Task {
        IrEffect::Task
    } else if left == IrEffect::Ordinary || right == IrEffect::Ordinary {
        IrEffect::Ordinary
    } else {
        left
    }
}

fn expression_effect(values: &[TypedExpressionIr]) -> IrEffect {
    values.iter().fold(IrEffect::Immediate, |effect, value| {
        max_effect(effect, value.effect)
    })
}

fn collect_expression_references(
    expression: &TypedExpressionIr,
    output: &mut BTreeSet<DefinitionId>,
) {
    match &expression.kind {
        TypedExpressionKind::Reference(definition) => {
            output.insert(*definition);
        }
        TypedExpressionKind::PersistentStateGet { state_type, .. } => {
            output.insert(*state_type);
        }
        TypedExpressionKind::Unary { operand, .. } => {
            collect_expression_references(operand, output);
        }
        TypedExpressionKind::Binary { left, right, .. } => {
            collect_expression_references(left, output);
            collect_expression_references(right, output);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => {
            for argument in arguments {
                collect_expression_references(argument, output);
            }
        }
        TypedExpressionKind::Construct { fields, .. } => {
            for (_, value) in fields {
                collect_expression_references(value, output);
            }
        }
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            if let Some(base) = update {
                collect_expression_references(base, output);
            }
            for (_, value) in fields {
                collect_expression_references(value, output);
            }
        }
        TypedExpressionKind::Update { base, fields } => {
            collect_expression_references(base, output);
            for (_, value) in fields {
                collect_expression_references(value, output);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                collect_expression_references(payload, output);
            }
        }
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            collect_expression_references(base, output);
        }
        TypedExpressionKind::Index { base, index } => {
            collect_expression_references(base, output);
            collect_expression_references(index, output);
        }
        TypedExpressionKind::Array(values)
        | TypedExpressionKind::Tuple(values)
        | TypedExpressionKind::StringInterpolation(values) => {
            for value in values {
                collect_expression_references(value, output);
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            collect_expression_references(value, output);
            for arm in arms {
                collect_expression_references(&arm.value, output);
            }
        }
        TypedExpressionKind::Try(value) | TypedExpressionKind::Await(value) => {
            collect_expression_references(value, output);
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. }
            | MigrationIntrinsicIr::Replace { target: object, .. } => {
                collect_expression_references(object, output);
            }
            MigrationIntrinsicIr::NewSet { object, value, .. } => {
                collect_expression_references(object, output);
                collect_expression_references(value, output);
            }
            MigrationIntrinsicIr::OldGet { .. }
            | MigrationIntrinsicIr::NewCreate { .. }
            | MigrationIntrinsicIr::Preserve { .. }
            | MigrationIntrinsicIr::Delete { .. }
            | MigrationIntrinsicIr::Finish => {}
        },
        TypedExpressionKind::Literal(_) | TypedExpressionKind::Yield => {}
    }
}

fn rewrite_expression_references(
    expression: &mut TypedExpressionIr,
    replacements: &BTreeMap<DefinitionId, DefinitionId>,
) {
    match &mut expression.kind {
        TypedExpressionKind::Reference(definition) => {
            if let Some(replacement) = replacements.get(definition) {
                *definition = *replacement;
            }
        }
        TypedExpressionKind::PersistentStateGet { state_type, .. } => {
            if let Some(replacement) = replacements.get(state_type) {
                *state_type = *replacement;
            }
        }
        TypedExpressionKind::Unary { operand, .. } => {
            rewrite_expression_references(operand, replacements);
        }
        TypedExpressionKind::Binary { left, right, .. } => {
            rewrite_expression_references(left, replacements);
            rewrite_expression_references(right, replacements);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => {
            for argument in arguments {
                rewrite_expression_references(argument, replacements);
            }
        }
        TypedExpressionKind::Construct { fields, .. } => {
            for (_, value) in fields {
                rewrite_expression_references(value, replacements);
            }
        }
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            if let Some(base) = update {
                rewrite_expression_references(base, replacements);
            }
            for (_, value) in fields {
                rewrite_expression_references(value, replacements);
            }
        }
        TypedExpressionKind::Update { base, fields } => {
            rewrite_expression_references(base, replacements);
            for (_, value) in fields {
                rewrite_expression_references(value, replacements);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                rewrite_expression_references(payload, replacements);
            }
        }
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            rewrite_expression_references(base, replacements);
        }
        TypedExpressionKind::Index { base, index } => {
            rewrite_expression_references(base, replacements);
            rewrite_expression_references(index, replacements);
        }
        TypedExpressionKind::Array(values)
        | TypedExpressionKind::Tuple(values)
        | TypedExpressionKind::StringInterpolation(values) => {
            for value in values {
                rewrite_expression_references(value, replacements);
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            rewrite_expression_references(value, replacements);
            for arm in arms {
                rewrite_expression_references(&mut arm.value, replacements);
            }
        }
        TypedExpressionKind::Try(value) | TypedExpressionKind::Await(value) => {
            rewrite_expression_references(value, replacements);
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. }
            | MigrationIntrinsicIr::Replace { target: object, .. } => {
                rewrite_expression_references(object, replacements);
            }
            MigrationIntrinsicIr::NewSet { object, value, .. } => {
                rewrite_expression_references(object, replacements);
                rewrite_expression_references(value, replacements);
            }
            MigrationIntrinsicIr::OldGet { .. }
            | MigrationIntrinsicIr::NewCreate { .. }
            | MigrationIntrinsicIr::Preserve { .. }
            | MigrationIntrinsicIr::Delete { .. }
            | MigrationIntrinsicIr::Finish => {}
        },
        TypedExpressionKind::Literal(_) | TypedExpressionKind::Yield => {}
    }
}

fn place_type(place: &TypedPlaceIr, definitions: &[Definition]) -> IrType {
    match place {
        TypedPlaceIr::Definition(definition) => definitions[definition.0 as usize].ty.clone(),
        TypedPlaceIr::Field { field, .. }
        | TypedPlaceIr::ClassField { field, .. }
        | TypedPlaceIr::StateField { field, .. } => definitions[field.0 as usize].ty.clone(),
        TypedPlaceIr::Index { base, .. } => match &base.ty {
            IrType::Array(inner) | IrType::Buffer(inner) => inner.as_ref().clone(),
            IrType::Map(_, value) => value.as_ref().clone(),
            IrType::String => IrType::Rune,
            _ => IrType::Unit,
        },
    }
}

fn restricted_name(operation: RestrictedOperation) -> &'static str {
    match operation {
        RestrictedOperation::Host => "Host",
        RestrictedOperation::Task => "Task",
        RestrictedOperation::Await => "await",
        RestrictedOperation::Yield => "yield",
        RestrictedOperation::Activation => "Activation",
        RestrictedOperation::Migration => "Migration",
        RestrictedOperation::PersistentState => "persistent State",
    }
}

fn definition_fingerprint_payload(
    definition: &Definition,
    signature: Option<&FunctionSignature>,
    constant: Option<&ConstValue>,
    metadata: Option<&TypeMetadata>,
    variant_payloads: &BTreeMap<DefinitionId, Vec<IrType>>,
    definitions: &[Definition],
) -> Vec<u8> {
    let mut payload = Vec::new();
    append_string(&mut payload, definition_kind_name(definition.kind));
    append_string(&mut payload, visibility_name(definition.visibility));
    append_string(&mut payload, effect_name(definition.effect));
    encode_type(&definition.ty, definitions, &mut payload);
    if let Some(signature) = signature {
        append_u32(
            &mut payload,
            u32::try_from(signature.parameter_types.len()).unwrap_or(u32::MAX),
        );
        for parameter in &signature.parameter_types {
            encode_type(parameter, definitions, &mut payload);
        }
        encode_type(&signature.result, definitions, &mut payload);
    }
    if let Some(constant) = constant {
        encode_const(constant, definitions, &mut payload);
    }
    if let Some(metadata) = metadata {
        append_u32(
            &mut payload,
            u32::try_from(metadata.field_order.len()).unwrap_or(u32::MAX),
        );
        for (source_order, field) in metadata.field_order.iter().enumerate() {
            append_u32(
                &mut payload,
                u32::try_from(source_order).unwrap_or(u32::MAX),
            );
            let field = &definitions[field.0 as usize];
            append_string(&mut payload, &field.canonical_identity);
            encode_type(&field.ty, definitions, &mut payload);
        }
        append_u32(
            &mut payload,
            u32::try_from(metadata.variant_order.len()).unwrap_or(u32::MAX),
        );
        for (tag, variant) in metadata.variant_order.iter().enumerate() {
            append_u32(&mut payload, u32::try_from(tag).unwrap_or(u32::MAX));
            append_string(
                &mut payload,
                &definitions[variant.0 as usize].canonical_identity,
            );
            let values = variant_payloads.get(variant).map_or(&[][..], Vec::as_slice);
            append_u32(
                &mut payload,
                u32::try_from(values.len()).unwrap_or(u32::MAX),
            );
            for value in values {
                encode_type(value, definitions, &mut payload);
            }
        }
    }
    payload
}

fn encode_type(ty: &IrType, definitions: &[Definition], output: &mut Vec<u8>) {
    match ty {
        IrType::Error => output.push(20),
        IrType::Unit => output.push(0),
        IrType::Bool => output.push(1),
        IrType::I32 => output.push(2),
        IrType::I64 => output.push(3),
        IrType::F32 => output.push(4),
        IrType::F64 => output.push(5),
        IrType::String => output.push(6),
        IrType::Rune => output.push(7),
        IrType::Named(definition) => {
            output.push(8);
            append_string(
                output,
                &definitions[definition.0 as usize].canonical_identity,
            );
        }
        IrType::Option(inner) => {
            output.push(9);
            encode_type(inner, definitions, output);
        }
        IrType::Result(ok, error) => {
            output.push(10);
            encode_type(ok, definitions, output);
            encode_type(error, definitions, output);
        }
        IrType::Array(inner) => {
            output.push(11);
            encode_type(inner, definitions, output);
        }
        IrType::Map(key, value) => {
            output.push(12);
            encode_type(key, definitions, output);
            encode_type(value, definitions, output);
        }
        IrType::Tuple(values) => {
            output.push(13);
            append_u32(output, u32::try_from(values.len()).unwrap_or(u32::MAX));
            for value in values {
                encode_type(value, definitions, output);
            }
        }
        IrType::HostRequest(inner) => {
            output.push(14);
            if let Some(inner) = inner {
                output.push(1);
                encode_type(inner, definitions, output);
            } else {
                output.push(0);
            }
        }
        IrType::ResourceToken(inner) => {
            output.push(15);
            if let Some(inner) = inner {
                output.push(1);
                encode_type(inner, definitions, output);
            } else {
                output.push(0);
            }
        }
        IrType::Snapshot(inner) => {
            output.push(16);
            encode_type(inner, definitions, output);
        }
        IrType::Buffer(inner) => {
            output.push(17);
            encode_type(inner, definitions, output);
        }
        IrType::StateHandle(inner) => {
            output.push(18);
            encode_type(inner, definitions, output);
        }
        IrType::Set(inner) => {
            output.push(21);
            encode_type(inner, definitions, output);
        }
        IrType::TypeParameter(index) => {
            output.push(19);
            output.extend_from_slice(&index.to_le_bytes());
        }
    }
}

fn encode_const(value: &ConstValue, definitions: &[Definition], output: &mut Vec<u8>) {
    match value {
        ConstValue::Unit => output.push(0),
        ConstValue::Bool(value) => {
            output.push(1);
            output.push(u8::from(*value));
        }
        ConstValue::I32(value) => {
            output.push(2);
            output.extend_from_slice(&value.to_le_bytes());
        }
        ConstValue::I64(value) => {
            output.push(3);
            output.extend_from_slice(&value.to_le_bytes());
        }
        ConstValue::F32(value) => {
            output.push(4);
            output.extend_from_slice(&value.to_le_bytes());
        }
        ConstValue::F64(value) => {
            output.push(5);
            output.extend_from_slice(&value.to_le_bytes());
        }
        ConstValue::String(value) => {
            output.push(6);
            append_string(output, value);
        }
        ConstValue::Rune(value) => {
            output.push(7);
            output.extend_from_slice(&u32::from(*value).to_le_bytes());
        }
        ConstValue::Tuple(values) => {
            output.push(8);
            append_u32(output, u32::try_from(values.len()).unwrap_or(u32::MAX));
            for value in values {
                encode_const(value, definitions, output);
            }
        }
        ConstValue::Construct { definition, fields } => {
            output.push(10);
            append_string(
                output,
                &definitions[definition.0 as usize].canonical_identity,
            );
            append_u32(output, u32::try_from(fields.len()).unwrap_or(u32::MAX));
            for (field, value) in fields {
                append_string(output, &definitions[field.0 as usize].canonical_identity);
                encode_const(value, definitions, output);
            }
        }
        ConstValue::Variant { definition, values } => {
            output.push(11);
            append_string(
                output,
                &definitions[definition.0 as usize].canonical_identity,
            );
            append_u32(output, u32::try_from(values.len()).unwrap_or(u32::MAX));
            for value in values {
                encode_const(value, definitions, output);
            }
        }
        ConstValue::BuiltinVariant { variant, value } => {
            output.push(12);
            output.push(match variant {
                BuiltinVariantIr::OptionSome => 0,
                BuiltinVariantIr::OptionNone => 1,
                BuiltinVariantIr::ResultOk => 2,
                BuiltinVariantIr::ResultErr => 3,
            });
            if let Some(value) = value {
                output.push(1);
                encode_const(value, definitions, output);
            } else {
                output.push(0);
            }
        }
    }
}

fn append_string(output: &mut Vec<u8>, value: &str) {
    append_u32(output, u32::try_from(value.len()).unwrap_or(u32::MAX));
    output.extend_from_slice(value.as_bytes());
}

fn append_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

const fn definition_kind_name(kind: DefinitionKind) -> &'static str {
    match kind {
        DefinitionKind::Function => "function",
        DefinitionKind::Task => "task",
        DefinitionKind::Struct => "struct",
        DefinitionKind::Enum => "enum",
        DefinitionKind::Class => "class",
        DefinitionKind::Const => "const",
        DefinitionKind::Field => "field",
        DefinitionKind::Variant => "variant",
        DefinitionKind::Parameter => "parameter",
        DefinitionKind::Local => "local",
        DefinitionKind::HostContract => "host-contract",
        DefinitionKind::HostFunction => "host-function",
        DefinitionKind::StandardLibrary => "standard-library",
    }
}

const fn is_nominal_type_kind(kind: DefinitionKind) -> bool {
    matches!(
        kind,
        DefinitionKind::Struct
            | DefinitionKind::Enum
            | DefinitionKind::Class
            | DefinitionKind::HostContract
            | DefinitionKind::StandardLibrary
    )
}

const fn effect_name(effect: IrEffect) -> &'static str {
    match effect {
        IrEffect::Ordinary => "ordinary",
        IrEffect::Immediate => "immediate",
        IrEffect::Task => "task",
        IrEffect::Migration => "migration",
        IrEffect::Activation => "activation",
        IrEffect::Cleanup => "cleanup",
    }
}

/// Whether the type (recursively) contains a poisoned [`IrType::Error`] from a failed
/// type/name resolution. Such types must not trigger cascading diagnostics.
#[must_use]
pub fn contains_ir_error(ty: &IrType) -> bool {
    match ty {
        IrType::Error => true,
        IrType::Option(inner)
        | IrType::Array(inner)
        | IrType::Set(inner)
        | IrType::Snapshot(inner)
        | IrType::Buffer(inner)
        | IrType::StateHandle(inner) => contains_ir_error(inner),
        IrType::HostRequest(inner) | IrType::ResourceToken(inner) => {
            inner.as_deref().is_some_and(contains_ir_error)
        }
        IrType::Result(ok, error) | IrType::Map(ok, error) => {
            contains_ir_error(ok) || contains_ir_error(error)
        }
        IrType::Tuple(items) => items.iter().any(contains_ir_error),
        IrType::Unit
        | IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune
        | IrType::Named(_)
        | IrType::TypeParameter(_) => false,
    }
}

/// Formats one fully resolved IR type using the canonical Nexa v2 source spelling.
#[must_use]
pub fn display_ir_type(ty: &IrType, definitions: &[Definition]) -> String {
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
        IrType::Named(definition) => definitions.get(definition.0 as usize).map_or_else(
            || format!("<definition {}>", definition.0),
            |value| value.name.clone(),
        ),
        IrType::Option(inner) => format!("Option<{}>", display_ir_type(inner, definitions)),
        IrType::Result(ok, error) => format!(
            "Result<{}, {}>",
            display_ir_type(ok, definitions),
            display_ir_type(error, definitions)
        ),
        IrType::Array(inner) => format!("Array<{}>", display_ir_type(inner, definitions)),
        IrType::Map(key, value) => format!(
            "Map<{}, {}>",
            display_ir_type(key, definitions),
            display_ir_type(value, definitions)
        ),
        IrType::Set(inner) => format!("Set<{}>", display_ir_type(inner, definitions)),
        IrType::Tuple(values) => format!(
            "({})",
            values
                .iter()
                .map(|value| display_ir_type(value, definitions))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        IrType::HostRequest(_) => "<runtime-only async request>".into(),
        IrType::ResourceToken(inner) => inner.as_ref().map_or_else(
            || "<invalid untyped Token>".into(),
            |inner| format!("Token<{}>", display_ir_type(inner, definitions)),
        ),
        IrType::Snapshot(inner) => format!("Snapshot<{}>", display_ir_type(inner, definitions)),
        IrType::Buffer(inner) => format!("Buffer<{}>", display_ir_type(inner, definitions)),
        IrType::StateHandle(inner) => {
            format!("StateHandle<{}>", display_ir_type(inner, definitions))
        }
        IrType::TypeParameter(index) => format!("<type-parameter {index}>"),
    }
}

fn surface_type_from_ir(
    ty: &IrType,
    definitions: &[Definition],
    type_parameters: &[String],
) -> Option<SurfaceType> {
    match ty {
        IrType::Error | IrType::HostRequest(_) | IrType::ResourceToken(None) => None,
        IrType::Unit => Some(SurfaceType::Unit),
        IrType::Bool => Some(SurfaceType::Bool),
        IrType::I32 => Some(SurfaceType::I32),
        IrType::I64 => Some(SurfaceType::I64),
        IrType::F32 => Some(SurfaceType::F32),
        IrType::F64 => Some(SurfaceType::F64),
        IrType::String => Some(SurfaceType::String),
        IrType::Rune => Some(SurfaceType::Rune),
        IrType::Named(definition) => {
            let definition = definitions.get(definition.0 as usize)?;
            Some(SurfaceType::Named {
                module: definition.module.clone(),
                name: definition.name.clone(),
            })
        }
        IrType::Option(inner) => Some(SurfaceType::Option(Box::new(surface_type_from_ir(
            inner,
            definitions,
            type_parameters,
        )?))),
        IrType::Result(ok, error) => Some(SurfaceType::Result(
            Box::new(surface_type_from_ir(ok, definitions, type_parameters)?),
            Box::new(surface_type_from_ir(error, definitions, type_parameters)?),
        )),
        IrType::Array(inner) => Some(SurfaceType::Array(Box::new(surface_type_from_ir(
            inner,
            definitions,
            type_parameters,
        )?))),
        IrType::Map(key, value) => Some(SurfaceType::Map(
            Box::new(surface_type_from_ir(key, definitions, type_parameters)?),
            Box::new(surface_type_from_ir(value, definitions, type_parameters)?),
        )),
        IrType::Set(inner) => Some(SurfaceType::Set(Box::new(surface_type_from_ir(
            inner,
            definitions,
            type_parameters,
        )?))),
        IrType::Tuple(values) => values
            .iter()
            .map(|value| surface_type_from_ir(value, definitions, type_parameters))
            .collect::<Option<Vec<_>>>()
            .map(SurfaceType::Tuple),
        IrType::ResourceToken(Some(inner)) => Some(SurfaceType::Token(Box::new(
            surface_type_from_ir(inner, definitions, type_parameters)?,
        ))),
        IrType::Snapshot(inner) => Some(SurfaceType::Snapshot(Box::new(surface_type_from_ir(
            inner,
            definitions,
            type_parameters,
        )?))),
        IrType::Buffer(inner) => Some(SurfaceType::Buffer(Box::new(surface_type_from_ir(
            inner,
            definitions,
            type_parameters,
        )?))),
        IrType::StateHandle(inner) => Some(SurfaceType::StateHandle(Box::new(
            surface_type_from_ir(inner, definitions, type_parameters)?,
        ))),
        IrType::TypeParameter(index) => type_parameters
            .get(usize::from(*index))
            .cloned()
            .map(SurfaceType::TypeParameter),
    }
}
