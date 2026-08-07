use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use nexa_core::StableId;
use nexa_syntax::ast::{
    self, DeclarationKind, ElseBranch, Expression, ExpressionKind, ForIterable, InterpolationPart,
    Pattern, PatternKind, Statement, StatementKind, TypeKind, TypeRef, UsePathRootKind,
    VariantPayload, parse_nexa_ast,
};
use nexa_syntax::parse_nexa;

use crate::{
    EffectiveEntrypointScanError, EffectiveEntrypointSet, ExternalTypeSurface, HostContractSurface,
    ModulePath, PackageId, PackageSourceSet, SourceKey, SurfaceType, effective_entrypoint_set,
};

/// The subset of a Host contract that can affect one package build.
///
/// Stable IDs are sorted and duplicate-free. `referenced_types` is transitively closed over
/// Host struct fields and enum variant payloads. `entrypoints` contains both required and
/// implemented optional Nexa entrypoints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectiveContractReferences {
    pub referenced_types: Vec<StableId>,
    pub host_functions: Vec<StableId>,
    pub entrypoints: EffectiveEntrypointSet,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectiveContractScanError {
    Entrypoints(EffectiveEntrypointScanError),
    SourceTooLarge(SourceKey),
    ReferencedTypeMissingStableId(String),
}

impl fmt::Display for EffectiveContractScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Entrypoints(error) => error.fmt(formatter),
            Self::SourceTooLarge(source) => write!(
                formatter,
                "source `{}/{}` exceeds the syntax limit while scanning Host references",
                source.package_id, source.path
            ),
            Self::ReferencedTypeMissingStableId(name) => write!(
                formatter,
                "referenced Host type `{name}` is missing a stable ABI ID"
            ),
        }
    }
}

impl Error for EffectiveContractScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Entrypoints(error) => Some(error),
            Self::SourceTooLarge(_) | Self::ReferencedTypeMissingStableId(_) => None,
        }
    }
}

impl From<EffectiveEntrypointScanError> for EffectiveContractScanError {
    fn from(value: EffectiveEntrypointScanError) -> Self {
        Self::Entrypoints(value)
    }
}

/// Pre-scans the complete linked source closure for the effective Host/Nexa contract surface.
///
/// The scan is intentionally syntax-error tolerant: it consumes every declaration and expression
/// recovered by the typed AST and leaves signature/type diagnostics to normal analysis. Host
/// references are recognized through the module's resolved import spelling, not through text
/// searches, so comments, strings, and similarly named non-Host namespaces do not participate.
///
/// Host function/type references are collected from the root package and every dependency
/// production source. Nexa entrypoints remain rooted exclusively in `entry_module` of the root
/// package: dependency functions cannot accidentally become application entrypoints.
///
/// A malformed implementation of an optional entrypoint still makes that entrypoint effective.
/// Its precise signature error is reported later by semantic analysis.
pub fn effective_contract_references(
    root_sources: &PackageSourceSet,
    dependency_sources: &BTreeMap<PackageId, Arc<PackageSourceSet>>,
    entry_module: &ModulePath,
    host: &HostContractSurface,
    required_names: &[String],
) -> Result<EffectiveContractReferences, EffectiveContractScanError> {
    let declared_entrypoints = host
        .nexa_entrypoints
        .iter()
        .map(|entrypoint| entrypoint.name.clone())
        .collect::<Vec<_>>();
    let entrypoints = effective_entrypoint_set(
        root_sources,
        entry_module,
        &declared_entrypoints,
        required_names,
    )?;

    let host_type_by_name = host
        .types
        .iter()
        .map(|surface| (surface.name.as_str(), surface))
        .collect::<BTreeMap<_, _>>();
    let host_function_by_name = host
        .functions
        .iter()
        .map(|surface| (surface.name.as_str(), surface))
        .collect::<BTreeMap<_, _>>();
    let contract_import_name = snake_case_name(&host.contract_name);

    let mut referenced_type_names = BTreeSet::<String>::new();
    let mut referenced_function_names = BTreeSet::<String>::new();
    for unit in std::iter::once(root_sources)
        .chain(dependency_sources.values().map(AsRef::as_ref))
        .flat_map(PackageSourceSet::production_units)
    {
        let syntax = parse_nexa(&unit.text)
            .map_err(|_| EffectiveContractScanError::SourceTooLarge(unit.key.clone()))?;
        let ast = parse_nexa_ast(&syntax);
        let aliases = ast
            .uses
            .iter()
            .filter(|usage| {
                usage.root.kind == UsePathRootKind::Host
                    && usage.segments.len() == 1
                    && usage.segments[0].text == contract_import_name
            })
            .map(|usage| {
                usage.alias.as_ref().map_or_else(
                    || usage.segments[0].text.clone(),
                    |alias| alias.text.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        let mut scanner = AstReferenceScanner {
            aliases: &aliases,
            contract_import_name: &contract_import_name,
            host_type_by_name: &host_type_by_name,
            host_function_by_name: &host_function_by_name,
            referenced_type_names: &mut referenced_type_names,
            referenced_function_names: &mut referenced_function_names,
        };
        scanner.scan_ast(&ast);
    }

    let mut host_functions = BTreeSet::new();
    for function_name in referenced_function_names {
        let Some(function) = host_function_by_name.get(function_name.as_str()) else {
            continue;
        };
        host_functions.insert(function.stable_id);
        for ty in &function.parameters {
            collect_named_host_types(ty, &mut referenced_type_names);
        }
        collect_named_host_types(&function.result, &mut referenced_type_names);
        if let Some(async_result) = &function.async_result {
            collect_named_host_types(&async_result.success, &mut referenced_type_names);
            collect_named_host_types(&async_result.error, &mut referenced_type_names);
        }
    }

    for entrypoint_name in &entrypoints.effective {
        for entrypoint in host
            .nexa_entrypoints
            .iter()
            .filter(|entrypoint| entrypoint.name == *entrypoint_name)
        {
            for ty in &entrypoint.parameters {
                collect_named_host_types(ty, &mut referenced_type_names);
            }
            collect_named_host_types(&entrypoint.result, &mut referenced_type_names);
        }
    }

    let referenced_types = close_referenced_types(&host_type_by_name, referenced_type_names)?;
    Ok(EffectiveContractReferences {
        referenced_types,
        host_functions: host_functions.into_iter().collect(),
        entrypoints,
    })
}

struct AstReferenceScanner<'a> {
    aliases: &'a BTreeSet<String>,
    contract_import_name: &'a str,
    host_type_by_name: &'a BTreeMap<&'a str, &'a ExternalTypeSurface>,
    host_function_by_name: &'a BTreeMap<&'a str, &'a crate::HostFunctionSurface>,
    referenced_type_names: &'a mut BTreeSet<String>,
    referenced_function_names: &'a mut BTreeSet<String>,
}

impl AstReferenceScanner<'_> {
    fn scan_ast(&mut self, ast: &ast::NexaAst) {
        for declaration in &ast.declarations {
            match &declaration.kind {
                DeclarationKind::Function(function) => {
                    for parameter in &function.parameters {
                        self.scan_type(&parameter.ty);
                    }
                    if let Some(result) = &function.result {
                        self.scan_type(result);
                    }
                    self.scan_block(&function.body);
                }
                DeclarationKind::Type(declaration) => {
                    for field in &declaration.fields {
                        self.scan_type(&field.ty);
                    }
                    for variant in &declaration.variants {
                        match &variant.payload {
                            VariantPayload::Unit => {}
                            VariantPayload::Tuple(types) => {
                                for ty in types {
                                    self.scan_type(ty);
                                }
                            }
                            VariantPayload::Struct(fields) => {
                                for field in fields {
                                    self.scan_type(&field.ty);
                                }
                            }
                        }
                    }
                }
                DeclarationKind::Const(declaration) => {
                    self.scan_type(&declaration.ty);
                    self.scan_expression(&declaration.value);
                }
                DeclarationKind::Error => {}
            }
        }
        for statement in &ast.top_level_statements {
            self.scan_statement(statement);
        }
        if let Some(tail) = &ast.top_level_tail {
            self.scan_expression(tail);
        }
    }

    fn scan_type(&mut self, ty: &TypeRef) {
        match &ty.kind {
            TypeKind::Named(path) => self.scan_type_path(path),
            TypeKind::Generic { base, arguments } => {
                self.scan_type_path(base);
                for ty in arguments {
                    self.scan_type(ty);
                }
            }
            TypeKind::Tuple(types) => {
                for ty in types {
                    self.scan_type(ty);
                }
            }
            TypeKind::Array(inner) | TypeKind::Option(inner) | TypeKind::Set(inner) => {
                self.scan_type(inner);
            }
            TypeKind::Map { key, value }
            | TypeKind::Result {
                ok: key,
                error: value,
            } => {
                self.scan_type(key);
                self.scan_type(value);
            }
            TypeKind::Error => {}
        }
    }

    fn scan_block(&mut self, block: &ast::Block) {
        for statement in &block.statements {
            self.scan_statement(statement);
        }
        if let Some(tail) = &block.tail {
            self.scan_expression(tail);
        }
    }

    fn scan_statement(&mut self, statement: &Statement) {
        match &statement.kind {
            StatementKind::Bind { ty, value, .. } => {
                if let Some(ty) = ty {
                    self.scan_type(ty);
                }
                self.scan_expression(value);
            }
            StatementKind::Assign { target, value }
            | StatementKind::CompoundAssign { target, value, .. } => {
                self.scan_expression(target);
                self.scan_expression(value);
            }
            StatementKind::Return(value) => {
                if let Some(value) = value {
                    self.scan_expression(value);
                }
            }
            StatementKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.scan_expression(condition);
                self.scan_block(then_block);
                if let Some(else_branch) = else_branch {
                    match else_branch {
                        ElseBranch::Block(block) => self.scan_block(block),
                        ElseBranch::If(statement) => self.scan_statement(statement),
                    }
                }
            }
            StatementKind::While { condition, body } => {
                self.scan_expression(condition);
                self.scan_block(body);
            }
            StatementKind::For { iterable, body, .. } => {
                self.scan_for_iterable(iterable);
                self.scan_block(body);
            }
            StatementKind::Defer(expression) | StatementKind::Expression(expression) => {
                self.scan_expression(expression);
            }
            StatementKind::Break
            | StatementKind::Continue
            | StatementKind::Yield
            | StatementKind::Error => {}
        }
    }

    fn scan_for_iterable(&mut self, iterable: &ForIterable) {
        match iterable {
            ForIterable::Range { start, end, .. } => {
                self.scan_expression(start);
                self.scan_expression(end);
            }
            ForIterable::Expression(expression) => self.scan_expression(expression),
        }
    }

    fn scan_expression(&mut self, expression: &Expression) {
        match &expression.kind {
            ExpressionKind::Literal(_) | ExpressionKind::Error => {}
            ExpressionKind::Name(path) => self.scan_type_path(path),
            ExpressionKind::Tuple(values) | ExpressionKind::Array(values) => {
                for value in values {
                    self.scan_expression(value);
                }
            }
            ExpressionKind::Unary { operand, .. }
            | ExpressionKind::Await { operand }
            | ExpressionKind::Try(operand) => self.scan_expression(operand),
            ExpressionKind::Binary { left, right, .. } => {
                self.scan_expression(left);
                self.scan_expression(right);
            }
            ExpressionKind::Call {
                callee,
                type_arguments,
                arguments,
            } => {
                if let ExpressionKind::Name(path) = &callee.kind {
                    self.scan_host_function_path(path);
                }
                self.scan_expression(callee);
                for ty in type_arguments {
                    self.scan_type(ty);
                }
                for argument in arguments {
                    self.scan_expression(argument);
                }
            }
            ExpressionKind::Member { receiver, .. } => self.scan_expression(receiver),
            ExpressionKind::Index { receiver, index } => {
                self.scan_expression(receiver);
                self.scan_expression(index);
            }
            ExpressionKind::Construct { ty, fields, update } => {
                self.scan_type_path(ty);
                for field in fields {
                    self.scan_expression(&field.value);
                }
                if let Some(update) = update {
                    self.scan_expression(update);
                }
            }
            ExpressionKind::New { ty, fields, update } => {
                self.scan_type(ty);
                for field in fields {
                    self.scan_expression(&field.value);
                }
                if let Some(update) = update {
                    self.scan_expression(update);
                }
            }
            ExpressionKind::Match { value, arms } => {
                self.scan_expression(value);
                for arm in arms {
                    self.scan_pattern(&arm.pattern);
                    self.scan_expression(&arm.value);
                }
            }
            ExpressionKind::Interpolation(parts) => {
                for part in parts {
                    if let InterpolationPart::Expression(expression) = part {
                        self.scan_expression(expression);
                    }
                }
            }
        }
    }

    fn scan_pattern(&mut self, pattern: &Pattern) {
        match &pattern.kind {
            PatternKind::Variant { path, payload } => {
                self.scan_type_path(path);
                for pattern in payload {
                    self.scan_pattern(pattern);
                }
            }
            PatternKind::Struct { path, fields } => {
                self.scan_type_path(path);
                for field in fields {
                    self.scan_pattern(&field.pattern);
                }
            }
            PatternKind::Wildcard
            | PatternKind::Binding(_)
            | PatternKind::Literal(_)
            | PatternKind::Error => {}
        }
    }

    fn scan_host_function_path(&mut self, path: &ast::QualifiedName) {
        let function = match path.segments.as_slice() {
            [alias, function] if self.aliases.contains(&alias.text) => function,
            [root, contract, function]
                if root.text == "host" && contract.text == self.contract_import_name =>
            {
                function
            }
            _ => return,
        };
        if self
            .host_function_by_name
            .contains_key(function.text.as_str())
        {
            self.referenced_function_names.insert(function.text.clone());
        }
    }

    fn scan_type_path(&mut self, path: &ast::QualifiedName) {
        let ty = match path.segments.as_slice() {
            [alias, ty, ..] if self.aliases.contains(&alias.text) => ty,
            [root, contract, ty, ..]
                if root.text == "host" && contract.text == self.contract_import_name =>
            {
                ty
            }
            _ => return,
        };
        if self.host_type_by_name.contains_key(ty.text.as_str()) {
            self.referenced_type_names.insert(ty.text.clone());
        }
    }
}

fn collect_named_host_types(ty: &SurfaceType, output: &mut BTreeSet<String>) {
    match ty {
        SurfaceType::Named { module, name } if module.as_str() == "host" => {
            output.insert(name.clone());
        }
        SurfaceType::Option(inner)
        | SurfaceType::Array(inner)
        | SurfaceType::Set(inner)
        | SurfaceType::Token(inner)
        | SurfaceType::Snapshot(inner)
        | SurfaceType::Buffer(inner)
        | SurfaceType::StateHandle(inner) => collect_named_host_types(inner, output),
        SurfaceType::Result(ok, error) | SurfaceType::Map(ok, error) => {
            collect_named_host_types(ok, output);
            collect_named_host_types(error, output);
        }
        SurfaceType::Tuple(types) => {
            for ty in types {
                collect_named_host_types(ty, output);
            }
        }
        SurfaceType::Unit
        | SurfaceType::Bool
        | SurfaceType::I32
        | SurfaceType::I64
        | SurfaceType::F32
        | SurfaceType::F64
        | SurfaceType::String
        | SurfaceType::Rune
        | SurfaceType::TypeParameter(_)
        | SurfaceType::Named { .. } => {}
    }
}

fn close_referenced_types(
    host_type_by_name: &BTreeMap<&str, &ExternalTypeSurface>,
    seed_names: BTreeSet<String>,
) -> Result<Vec<StableId>, EffectiveContractScanError> {
    let mut queue = seed_names.into_iter().collect::<VecDeque<_>>();
    let mut visited = BTreeSet::<String>::new();
    let mut stable_ids = BTreeSet::new();
    while let Some(name) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(surface) = host_type_by_name.get(name.as_str()) else {
            // Normal semantic analysis owns the precise unknown-type diagnostic.
            continue;
        };
        let stable_id = surface.stable_id.ok_or_else(|| {
            EffectiveContractScanError::ReferencedTypeMissingStableId(name.clone())
        })?;
        stable_ids.insert(stable_id);

        let mut nested = BTreeSet::new();
        for field in &surface.fields {
            collect_named_host_types(&field.ty, &mut nested);
        }
        for variant in &surface.variants {
            for ty in &variant.payload {
                collect_named_host_types(ty, &mut nested);
            }
        }
        queue.extend(nested);
    }
    Ok(stable_ids.into_iter().collect())
}

fn snake_case_name(value: &str) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    let mut output = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        if character.is_ascii_uppercase() {
            let previous_is_lower_or_digit = index > 0
                && (characters[index - 1].is_ascii_lowercase()
                    || characters[index - 1].is_ascii_digit());
            let acronym_boundary = index > 0
                && characters[index - 1].is_ascii_uppercase()
                && characters
                    .get(index + 1)
                    .is_some_and(char::is_ascii_lowercase);
            if !output.is_empty() && (previous_is_lower_or_digit || acronym_boundary) {
                output.push('_');
            }
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        CompilationLimits, ExternalFieldSurface, ExternalTypeKind, ExternalVariantSurface,
        HostFunctionMode, NexaEntrypointSurface, NormalizedPackagePath, PackageId,
        RequiredEntrypointSurface, SourceRole, SourceSetBuilder,
    };

    use super::*;

    fn host_type(
        name: &str,
        stable_id: u64,
        fields: Vec<SurfaceType>,
        variants: Vec<Vec<SurfaceType>>,
    ) -> ExternalTypeSurface {
        ExternalTypeSurface {
            name: name.into(),
            kind: if variants.is_empty() {
                ExternalTypeKind::Struct
            } else {
                ExternalTypeKind::Enum
            },
            stable_id: Some(StableId(stable_id)),
            type_parameters: Vec::new(),
            fields: fields
                .into_iter()
                .enumerate()
                .map(|(index, ty)| ExternalFieldSurface {
                    name: format!("field_{index}"),
                    stable_id: None,
                    ty,
                    source: None,
                })
                .collect(),
            variants: variants
                .into_iter()
                .enumerate()
                .map(|(index, payload)| ExternalVariantSurface {
                    name: format!("Variant{index}"),
                    stable_id: None,
                    payload,
                    source: None,
                })
                .collect(),
            source: None,
        }
    }

    fn sources(text: &str) -> PackageSourceSet {
        let mut builder = SourceSetBuilder::new(
            PackageId::new("example.contract_scan").unwrap(),
            CompilationLimits::default(),
        );
        builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                Arc::<str>::from(text),
                SourceRole::Production,
            )
            .unwrap();
        builder.build().unwrap()
    }

    fn host() -> HostContractSurface {
        let named = |name: &str| SurfaceType::Named {
            module: ModulePath::new("host").unwrap(),
            name: name.into(),
        };
        HostContractSurface {
            contract_name: "GameHTTPHost".into(),
            contract_stable_id: StableId(1),
            types: vec![
                host_type("Direct", 10, Vec::new(), Vec::new()),
                host_type("Envelope", 20, vec![named("Payload")], Vec::new()),
                host_type(
                    "Payload",
                    30,
                    Vec::new(),
                    vec![vec![SurfaceType::Option(Box::new(named("Failure")))]],
                ),
                host_type("Failure", 40, Vec::new(), Vec::new()),
                host_type("Unused", 50, Vec::new(), Vec::new()),
            ],
            functions: vec![
                crate::HostFunctionSurface {
                    name: "load".into(),
                    parameters: vec![named("Envelope")],
                    result: named("Payload"),
                    mode: HostFunctionMode::Sync,
                    stable_id: StableId(100),
                    declaration_fingerprint: [1; 32],
                    import_index: 0,
                    fuel_cost: 1,
                    async_result: None,
                    required_capabilities: Vec::new(),
                    source: None,
                },
                crate::HostFunctionSurface {
                    name: "ignored".into(),
                    parameters: vec![named("Unused")],
                    result: SurfaceType::Unit,
                    mode: HostFunctionMode::Sync,
                    stable_id: StableId(200),
                    declaration_fingerprint: [2; 32],
                    import_index: 1,
                    fuel_cost: 1,
                    async_result: None,
                    required_capabilities: Vec::new(),
                    source: None,
                },
            ],
            nexa_entrypoints: vec![
                NexaEntrypointSurface {
                    name: "required_tick".into(),
                    stable_id: StableId(300),
                    parameters: vec![named("Failure")],
                    result: SurfaceType::Unit,
                    effect: None,
                    source: None,
                },
                NexaEntrypointSurface {
                    name: "optional_tick".into(),
                    stable_id: StableId(301),
                    parameters: vec![named("Direct")],
                    result: SurfaceType::Unit,
                    effect: None,
                    source: None,
                },
                NexaEntrypointSurface {
                    name: "unused_tick".into(),
                    stable_id: StableId(302),
                    parameters: vec![named("Unused")],
                    result: SurfaceType::Unit,
                    effect: None,
                    source: None,
                },
            ],
            required_entrypoints: vec![RequiredEntrypointSurface {
                name: "required_tick".into(),
                stable_id: StableId(300),
                parameters: vec![named("Failure")],
                result: SurfaceType::Unit,
                effect: None,
                source: None,
            }],
            source: None,
        }
    }

    #[test]
    fn qualified_host_references_form_a_transitive_deterministic_closure() {
        let references = effective_contract_references(
            &sources(
                r#"
use host::game_http_host as api;

pub fn optional_tick(value: Missing) -> unit {
    let direct: api::Direct = api::Direct {};
    api::load();
    let text = "api::ignored()";
    // api::ignored();
}
"#,
            ),
            &BTreeMap::new(),
            &ModulePath::new("main").unwrap(),
            &host(),
            &["required_tick".into()],
        )
        .unwrap();

        assert_eq!(references.host_functions, [StableId(100)]);
        assert_eq!(
            references.referenced_types,
            [StableId(10), StableId(20), StableId(30), StableId(40)]
        );
        assert_eq!(references.entrypoints.required, ["required_tick"]);
        assert_eq!(
            references.entrypoints.implemented_optional,
            ["optional_tick"]
        );
        assert_eq!(
            references.entrypoints.effective,
            ["optional_tick", "required_tick"]
        );
    }

    #[test]
    fn similarly_spelled_non_host_paths_do_not_participate() {
        let references = effective_contract_references(
            &sources(
                r"
use package::game_http_host as api;

pub fn run() {
    api::ignored();
}
",
            ),
            &BTreeMap::new(),
            &ModulePath::new("main").unwrap(),
            &host(),
            &[],
        )
        .unwrap();

        assert!(references.host_functions.is_empty());
        assert!(references.referenced_types.is_empty());
    }

    #[test]
    fn dependency_and_direct_host_paths_participate_without_changing_root_entrypoints() {
        let root = sources(
            r"
pub fn optional_tick(value: unit) -> unit {
}
",
        );
        let mut dependency_builder = SourceSetBuilder::new(
            PackageId::new("example.contract_dependency").unwrap(),
            CompilationLimits::default(),
        );
        dependency_builder
            .add(
                NormalizedPackagePath::new("src/lib.nexa").unwrap(),
                Arc::<str>::from(
                    r"
pub fn dependency_value(value: host::game_http_host::Direct) -> unit {
    host::game_http_host::load();
}
",
                ),
                SourceRole::Production,
            )
            .unwrap();
        let dependencies = BTreeMap::from([(
            PackageId::new("example.contract_dependency").unwrap(),
            Arc::new(dependency_builder.build().unwrap()),
        )]);

        let references = effective_contract_references(
            &root,
            &dependencies,
            &ModulePath::new("main").unwrap(),
            &host(),
            &[],
        )
        .unwrap();

        assert_eq!(references.host_functions, [StableId(100)]);
        assert_eq!(
            references.referenced_types,
            [StableId(10), StableId(20), StableId(30), StableId(40)]
        );
        assert_eq!(
            references.entrypoints.implemented_optional,
            ["optional_tick"]
        );
        assert_eq!(references.entrypoints.effective, ["optional_tick"]);
    }
}
