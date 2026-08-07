use nexa_syntax::ast::{
    DeclarationKind, ForBindings, ForIterable, StatementKind, TypeKind, parse_nexa_ast,
};
use nexa_syntax::parse_nexa;

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
