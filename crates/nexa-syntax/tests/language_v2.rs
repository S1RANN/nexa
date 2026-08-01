use nexa_syntax::ast::{
    AttributeArgumentClassification, DeclarationKind, ExpressionKind, StatementKind, parse_nexa_ast,
};
use nexa_syntax::{
    CellCompleteness, Keyword, TokenKind, classify_cell_completeness, lex_nexa, parse_nexa,
};

#[test]
fn language_v2_positive_surface_matrix() {
    let source = r"
use package::model;
use self::helpers as helpers;
use super::shared;
use host::console;
use std::math;
use snake_common::score;

@state(version = 1)
class State {
    mut score: i32,
}

struct Cell {
    x: i32,
    y: i32,
}

enum Direction {
    Up,
    Down,
    Teleport {
        cell: Cell,
    },
}

const MAX_SCORE: i32 = 100;

async fn load_score(id: i32) -> Result<i32, string> {
    let mut values = Array::new();
    let moved = Cell { x: 10, ..cell };
    let copied = new State { score: 50, ..state };
    values[0] = host::console::load(id).await?.score;
    return moved.x + copied.score;
}

let mut script_score = 0;
script_score = script_score + 1;
";
    let tree = parse_nexa(source).expect("small source");
    assert!(tree.errors.is_empty(), "tree errors: {:?}", tree.errors);
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "AST errors: {:?}", ast.errors);
    assert_eq!(ast.uses.len(), 6);
    assert_eq!(ast.declarations.len(), 5);
    assert_eq!(ast.top_level_statements.len(), 2);
    let DeclarationKind::Function(function) = &ast.declarations[4].kind else {
        panic!("async function");
    };
    assert!(function.is_async);
    assert!(
        function.body.statements.iter().any(|statement| {
            matches!(statement.kind, StatementKind::Bind { mutable: true, .. })
        })
    );
    assert!(function.body.statements.iter().any(|statement| {
        let StatementKind::Assign { value, .. } = &statement.kind else {
            return false;
        };
        let ExpressionKind::Member { receiver, .. } = &value.kind else {
            return false;
        };
        let ExpressionKind::Try(receiver) = &receiver.kind else {
            return false;
        };
        matches!(receiver.kind, ExpressionKind::Await { .. })
    }));
}

#[test]
fn removed_nexa_words_are_plain_identifiers_in_the_lexer() {
    let removed = [
        "var",
        "module",
        "import",
        "task",
        "immediate",
        "migration",
        "activation",
        "cleanup",
        "stateful",
        "with",
    ];
    for word in removed {
        let lexed = lex_nexa(word).expect("small source");
        assert_eq!(lexed.tokens.len(), 1, "{word}");
        assert_eq!(lexed.tokens[0].kind, TokenKind::Identifier, "{word}");
    }
    let added = [
        ("mut", Keyword::Mut),
        ("use", Keyword::Use),
        ("async", Keyword::Async),
    ];
    for (word, keyword) in added {
        let lexed = lex_nexa(word).expect("small source");
        assert_eq!(lexed.tokens[0].kind, TokenKind::Keyword(keyword), "{word}");
    }
}

#[test]
fn legacy_surface_forms_are_rejected_at_the_removed_word() {
    let cases = [
        ("module app::main;", "module"),
        ("import host::snake;", "import"),
        ("task fn run() {}", "task"),
        ("immediate fn run() {}", "immediate"),
        ("migration fn run() {}", "migration"),
        ("activation fn run() {}", "activation"),
        ("cleanup fn run() {}", "cleanup"),
        ("stateful class State {}", "stateful"),
        ("var value = 1;", "var"),
        ("await load();", "prefix `await`"),
        ("value with { score: 1 };", "`with` update"),
    ];
    for (source, expected) in cases {
        let tree = parse_nexa(source).expect("small source");
        let ast = parse_nexa_ast(&tree);
        let error = ast
            .errors
            .iter()
            .find(|error| error.message.contains(expected))
            .unwrap_or_else(|| panic!("{source:?} errors: {:?}", ast.errors));
        let word = source.split_ascii_whitespace().next().expect("first word");
        if expected == "`with` update" {
            assert_eq!(
                &source[usize::try_from(error.range.start.get()).unwrap()
                    ..usize::try_from(error.range.end.get()).unwrap()],
                "with"
            );
        } else {
            assert_eq!(
                error.range.start.get(),
                0,
                "{source:?} should point at {word:?}"
            );
        }
    }
}

#[test]
fn mut_is_only_valid_for_let_and_class_fields() {
    let accepted = parse_nexa("class State { mut score: i32, }\nfn run() { let mut score = 0; }\n")
        .expect("small source");
    let ast = parse_nexa_ast(&accepted);
    assert!(ast.errors.is_empty(), "{:?}", ast.errors);

    for source in [
        "struct Cell { mut x: i32, }",
        "enum Value { mut Some(i32), }",
    ] {
        let tree = parse_nexa(source).expect("small source");
        let ast = parse_nexa_ast(&tree);
        let error = ast
            .errors
            .iter()
            .find(|error| error.message.contains("only allowed on class fields"))
            .unwrap_or_else(|| panic!("{source}: {:?}", ast.errors));
        assert_eq!(
            &source[usize::try_from(error.range.start.get()).unwrap()
                ..usize::try_from(error.range.end.get()).unwrap()],
            "mut"
        );
    }
}

#[test]
fn prefix_await_is_rejected_but_full_postfix_chain_is_preserved() {
    let source = r"
async fn run() -> i32 {
    let value = client::connect().await?.fetch().await?[0];
    return value;
}
";
    let tree = parse_nexa(source).expect("small source");
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "{:?}", ast.errors);

    for source in [
        "async fn run() { await load(); }",
        r#"async fn run() { let x = "${await load()}"; }"#,
    ] {
        let prefix = parse_nexa(source).expect("small source");
        let prefix = parse_nexa_ast(&prefix);
        assert!(
            prefix
                .errors
                .iter()
                .any(|error| error.message.contains("prefix `await`")),
            "{source:?} errors: {:?}",
            prefix.errors
        );
    }
}

#[test]
fn named_attribute_arguments_preserve_names_and_classification() {
    let source = r"
@state(version = 1, extra = 2, version = 3)
class State {
    mut score: i32,
}
";
    let tree = parse_nexa(source).expect("small source");
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "{:?}", ast.errors);
    let arguments = &ast.declarations[0].attributes[0].arguments;
    assert_eq!(
        arguments
            .iter()
            .map(|argument| argument.classification)
            .collect::<Vec<_>>(),
        [
            AttributeArgumentClassification::Named,
            AttributeArgumentClassification::Unknown,
            AttributeArgumentClassification::Duplicate,
        ]
    );
    assert_eq!(
        arguments
            .iter()
            .map(|argument| argument.text.as_str())
            .collect::<Vec<_>>(),
        ["1", "2", "3"]
    );
}

#[test]
fn naming_rules_have_stable_diagnostics() {
    let source = r"
struct bad_type {
    BadField: i32,
}
const bad_const: i32 = 0;
fn BadFunction(BadParameter: i32) {
    let BadLocal = 0;
}
";
    let tree = parse_nexa(source).expect("small source");
    let ast = parse_nexa_ast(&tree);
    for expected in [
        "type name must use PascalCase",
        "field name must use snake_case",
        "const name must use SCREAMING_SNAKE_CASE",
        "function name must use snake_case",
        "parameter name must use snake_case",
        "local variable name must use snake_case",
    ] {
        assert!(
            ast.errors.iter().any(|error| error.message == expected),
            "missing {expected:?}: {:?}",
            ast.errors
        );
    }
}

#[test]
fn script_eof_tail_is_distinct_from_a_semicolon_statement() {
    let tail_tree = parse_nexa("1 + 2\n").expect("small source");
    let tail = parse_nexa_ast(&tail_tree);
    assert!(tail.errors.is_empty(), "{:?}", tail.errors);
    assert!(tail.top_level_statements.is_empty());
    assert!(matches!(
        tail.top_level_tail
            .as_deref()
            .map(|expression| &expression.kind),
        Some(ExpressionKind::Binary { .. })
    ));

    let statement_tree = parse_nexa("1 + 2;").expect("small source");
    let statement = parse_nexa_ast(&statement_tree);
    assert!(statement.errors.is_empty(), "{:?}", statement.errors);
    assert_eq!(statement.top_level_statements.len(), 1);
    assert!(matches!(
        statement.top_level_statements[0].kind,
        StatementKind::Expression(_)
    ));
    assert!(statement.top_level_tail.is_none());
}

#[test]
fn repl_cell_completeness_uses_canonical_lexical_structure() {
    for source in [
        "fn value() {\n",
        "let values = [1, 2,\n",
        "let value = (1 + 2\n",
        "let text = \"unfinished",
        "/* unfinished",
    ] {
        assert_eq!(
            classify_cell_completeness(source).expect("small source"),
            CellCompleteness::Incomplete,
            "{source:?}"
        );
    }

    for source in [
        "1 + 2\n",
        "let value = ;",
        "fn value(] {}",
        "\"invalid\\q\"",
    ] {
        assert_eq!(
            classify_cell_completeness(source).expect("small source"),
            CellCompleteness::Complete,
            "{source:?}"
        );
    }
}
