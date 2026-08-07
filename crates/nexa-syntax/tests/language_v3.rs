use nexa_syntax::ast::{
    DeclarationKind, ExpressionKind, ForBindings, ForIterable, StatementKind, TypeDeclarationKind,
    TypeKind, parse_nexa_ast,
};
use nexa_syntax::parse_nexa;

#[test]
fn function_generics_preserve_declared_type_parameters() {
    let tree = parse_nexa("fn pair<T, U>(left: T, right: U) -> T { return left; }\n")
        .expect("valid generic function source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "{:?}", ast.errors);
    let DeclarationKind::Function(function) = &ast.declarations[0].kind else {
        panic!("expected a function declaration");
    };
    assert_eq!(
        function
            .type_parameters
            .iter()
            .map(|parameter| parameter.name.text.as_str())
            .collect::<Vec<_>>(),
        ["T", "U"]
    );
}

#[test]
fn function_generics_parse_inline_and_where_bounds() {
    let tree = parse_nexa(
        "fn smaller<T: Copy>(left: T, right: T) -> T where T: PartialOrd + Display, { return left; }\n",
    )
    .expect("valid bounded generic function source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "{:?}", ast.errors);
    let DeclarationKind::Function(function) = &ast.declarations[0].kind else {
        panic!("expected a function declaration");
    };
    assert_eq!(function.type_parameters[0].bounds[0].name.text, "Copy");
    assert_eq!(
        function.where_constraints[0]
            .bounds
            .iter()
            .map(|bound| bound.name.text.as_str())
            .collect::<Vec<_>>(),
        ["PartialOrd", "Display"]
    );
}

#[test]
fn generic_operator_bounds_parse_closed_output_types() {
    let tree = parse_nexa(
        "fn add<T>(left: T, right: T) -> T where T: Add<Output = T> + Neg<Output = T> { return left + right; }\n",
    )
    .expect("valid generic operator bounds");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "{:?}", ast.errors);
    let DeclarationKind::Function(function) = &ast.declarations[0].kind else {
        panic!("expected a function declaration");
    };
    assert_eq!(function.where_constraints[0].bounds[0].name.text, "Add");
    assert!(function.where_constraints[0].bounds[0].output.is_some());
    assert_eq!(function.where_constraints[0].bounds[1].name.text, "Neg");
    assert!(function.where_constraints[0].bounds[1].output.is_some());
}

#[test]
fn generic_nominal_declarations_and_explicit_constructors_parse() {
    let tree = parse_nexa(
        r#"
struct Pair<T, U> {
    first: T,
    second: U,
}

enum Maybe<T> {
    None,
    Some(T),
}

class Box<T> {
    value: T,
}

fn main() {
    let pair = Pair<string, i32> { first: "score", second: 10 };
    let value = Maybe<i32>::Some(20);
    let boxed = Box<i32> { value: 30 };
}
"#,
    )
    .expect("valid generic nominal declarations");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "{:?}", ast.errors);
    for (index, kind) in [
        TypeDeclarationKind::Struct,
        TypeDeclarationKind::Enum,
        TypeDeclarationKind::Class,
    ]
    .into_iter()
    .enumerate()
    {
        let DeclarationKind::Type(declaration) = &ast.declarations[index].kind else {
            panic!("expected a generic type declaration");
        };
        assert_eq!(declaration.kind, kind);
        assert!(!declaration.type_parameters.is_empty());
    }
    let DeclarationKind::Function(main) = &ast.declarations[3].kind else {
        panic!("expected main function");
    };
    let StatementKind::Bind { value: pair, .. } = &main.body.statements[0].kind else {
        panic!("expected Pair binding");
    };
    assert!(matches!(
        pair.kind,
        ExpressionKind::Construct {
            ref type_arguments,
            ..
        } if type_arguments.len() == 2
    ));
    let StatementKind::Bind { value, .. } = &main.body.statements[1].kind else {
        panic!("expected Maybe binding");
    };
    assert!(matches!(
        value.kind,
        ExpressionKind::Call {
            ref type_arguments,
            ..
        } if type_arguments.len() == 1
    ));
}

fn for_statement(source: &str) -> (ForBindings, ForIterable) {
    let tree = parse_nexa(source).expect("valid source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_nexa_ast(&tree);
    assert_eq!(ast.errors.len(), 0, "{:?}", ast.errors);
    let DeclarationKind::Function(function) = &ast.declarations[0].kind else {
        panic!(
            "expected a function declaration, got {:#?}",
            ast.declarations[0].kind
        );
    };
    let statement = function
        .body
        .statements
        .first()
        .expect("one statement in function body");
    let StatementKind::For {
        bindings, iterable, ..
    } = &statement.kind
    else {
        panic!("expected a for statement, got {:#?}", statement.kind);
    };
    (bindings.clone(), iterable.clone())
}

#[test]
fn static_range_for_stays_lossless_with_one_binding() {
    let (bindings, iterable) = for_statement("fn run() { for i in 0..10 { } }\n");
    assert!(matches!(bindings, ForBindings::Single(binding) if binding.text == "i"));
    let ForIterable::Range { start, end, .. } = iterable else {
        panic!("expected an explicit Range iterable");
    };
    assert!(matches!(
        start.kind,
        nexa_syntax::ast::ExpressionKind::Literal(_)
    ));
    assert!(matches!(
        end.kind,
        nexa_syntax::ast::ExpressionKind::Literal(_)
    ));
}

#[test]
fn dynamic_range_for_preserves_expression_endpoints() {
    let (bindings, iterable) = for_statement("fn run(n: i32) { for i in 0..n { } }\n");
    assert!(matches!(bindings, ForBindings::Single(binding) if binding.text == "i"));
    let ForIterable::Range { start, end, .. } = iterable else {
        panic!("expected an explicit Range iterable");
    };
    assert!(matches!(
        start.kind,
        nexa_syntax::ast::ExpressionKind::Literal(_)
    ));
    assert!(matches!(
        end.kind,
        nexa_syntax::ast::ExpressionKind::Name(ref path) if path.text() == "n"
    ));
}

#[test]
fn collection_for_preserves_lossless_iterable_expression() {
    let (bindings, iterable) = for_statement("fn run() { for item in lookup() { } }\n");
    assert!(matches!(bindings, ForBindings::Single(binding) if binding.text == "item"));
    let ForIterable::Expression(expression) = iterable else {
        panic!("expected a generic iterable expression");
    };
    assert!(matches!(
        expression.kind,
        nexa_syntax::ast::ExpressionKind::Call { .. }
    ));
}

#[test]
fn pair_bindings_parse_for_map_iteration() {
    let (bindings, iterable) = for_statement("fn run() { for (key, value) in scores { } }\n");
    let ForBindings::Pair { key, value, range } = bindings else {
        panic!("expected a Pair binding");
    };
    assert_eq!(key.text, "key");
    assert_eq!(value.text, "value");
    assert!(range.end > range.start);
    assert!(matches!(iterable, ForIterable::Expression(_)));
}

#[test]
fn set_type_parses_as_builtin_generic() {
    let tree = parse_nexa("fn run() { let s: Set<i32> = Set::new(); }\n").expect("valid source");
    let ast = parse_nexa_ast(&tree);
    assert_eq!(ast.errors.len(), 0, "{:?}", ast.errors);
    let DeclarationKind::Function(function) = &ast.declarations[0].kind else {
        panic!("expected a function declaration");
    };
    let StatementKind::Bind { ty: Some(ty), .. } = &function.body.statements[0].kind else {
        panic!("expected a typed binding");
    };
    assert!(matches!(ty.kind, TypeKind::Set(_)), "{:#?}", ty.kind);
}

#[test]
fn empty_set_generic_keeps_arity_error_surface() {
    let tree = parse_nexa("fn run() { let s: Set = Set::new(); }\n").expect("valid source");
    let ast = parse_nexa_ast(&tree);
    let DeclarationKind::Function(function) = &ast.declarations[0].kind else {
        panic!("expected a function declaration");
    };
    let StatementKind::Bind { ty: Some(ty), .. } = &function.body.statements[0].kind else {
        panic!("expected a typed binding");
    };
    assert!(
        matches!(ty.kind, TypeKind::Generic { ref base, .. } if base.text() == "Set"),
        "{:#?}",
        ty.kind
    );
}
