use nexa_syntax::ast::{
    DeclarationKind, ExpressionKind, FunctionEffect, InterpolationPart, PatternKind, StatementKind,
    TypeDeclarationKind, TypeKind, Visibility, parse_nexa_ast,
};
use nexa_syntax::parse_nexa;
use std::path::{Path, PathBuf};

#[test]
#[allow(clippy::too_many_lines)]
fn complete_ast_covers_existing_and_m4_body_syntax() {
    let source = r#"
module demo.main;
import shared.types as types;
import host as snake;

/// Public default score.
@stable("default-score")
pub(package) const DEFAULT_SCORE: i32 = 10 + 2;

@stateful(2)
pub class GameState {
    /// Stable score field.
    @stable("score")
    score: i32;
}

pub enum Food {
    Normal,
    Poison(i32),
}

@test
fn arithmetic_is_stable() -> bool {
    return DEFAULT_SCORE == 12;
}

task fn run(input: Option<i32>) -> Result<i32, string> {
    let score: i32 = input.unwrap_or(0);
    var array: Array<i32> = [1, 2, 3];
    let empty = Array.new<i32>();
    let state = new GameState { score: score };
    let changed = state with { score: score + 1 };
    array[0] = changed.score;
    defer debug.assert(true);
    if score > 0 {
        while array.len() > 0 {
            break;
        }
    } else if score == 0 {
        continue;
    } else {
        yield;
    }
    for index in 0..10 {
        let text: string = "score=${score}, literal=\${score}";
        snake.consume(index);
    }
    let loaded = await snake.load(score)?;
    return match loaded {
        Some(found) => found + 1,
        Result.Ok(value) => value,
        Food { score: amount } => amount,
        _ => 0,
    };
}
"#;
    let tree = parse_nexa(source).expect("small source");
    assert!(tree.errors.is_empty(), "tree errors: {:?}", tree.errors);
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "AST errors: {:?}", ast.errors);
    assert_eq!(
        ast.module.as_ref().map(|module| module.path.text()),
        Some("demo.main".into())
    );
    assert_eq!(ast.imports.len(), 2);
    assert_eq!(ast.declarations.len(), 5);

    let constant = &ast.declarations[0];
    assert_eq!(constant.visibility, Visibility::Package);
    assert_eq!(constant.docs[0].text, "/// Public default score.");
    assert_eq!(constant.attributes[0].name.text, "stable");
    assert_eq!(
        constant.attributes[0].arguments[0].text,
        "\"default-score\""
    );

    let DeclarationKind::Type(state) = &ast.declarations[1].kind else {
        panic!("expected state type");
    };
    assert_eq!(state.kind, TypeDeclarationKind::Stateful);
    assert_eq!(state.fields[0].attributes[0].name.text, "stable");

    let DeclarationKind::Function(test) = &ast.declarations[3].kind else {
        panic!("expected test function");
    };
    assert_eq!(test.effect, FunctionEffect::Ordinary);
    assert_eq!(ast.declarations[3].attributes[0].name.text, "test");

    let DeclarationKind::Function(run) = &ast.declarations[4].kind else {
        panic!("expected task function");
    };
    assert_eq!(run.effect, FunctionEffect::Task);
    assert!(matches!(run.parameters[0].ty.kind, TypeKind::Option(_)));
    assert!(matches!(
        run.result.as_ref().map(|result| &result.kind),
        Some(TypeKind::Result { .. })
    ));
    assert!(
        run.body
            .statements
            .iter()
            .any(|statement| matches!(statement.kind, StatementKind::For { .. }))
    );
    assert!(
        run.body
            .statements
            .iter()
            .any(|statement| matches!(statement.kind, StatementKind::If { .. }))
    );

    let return_statement = run
        .body
        .statements
        .iter()
        .find_map(|statement| match &statement.kind {
            StatementKind::Return(Some(expression)) => Some(expression),
            _ => None,
        })
        .expect("match return");
    let ExpressionKind::Match { arms, .. } = &return_statement.kind else {
        panic!("expected match expression");
    };
    assert!(matches!(arms[0].pattern.kind, PatternKind::Variant { .. }));
    assert!(matches!(arms[2].pattern.kind, PatternKind::Struct { .. }));
    assert!(matches!(arms[3].pattern.kind, PatternKind::Wildcard));
}

#[test]
fn ast_recovery_preserves_later_function_bodies() {
    let source = r"
module recovery.main;
fn broken() -> i32 {
    let value = ;
    return 0;
}
pub fn recovered(value: Map<string, Array<i32>>) -> (i32, bool) {
    return (value.len(), true);
}
";
    let tree = parse_nexa(source).expect("small source");
    let ast = parse_nexa_ast(&tree);
    assert!(!ast.errors.is_empty());
    let recovered = ast
        .declarations
        .iter()
        .find_map(|declaration| match &declaration.kind {
            DeclarationKind::Function(function) if function.name.text == "recovered" => {
                Some(function)
            }
            _ => None,
        })
        .expect("later function survives recovery");
    assert_eq!(recovered.parameters.len(), 1);
    assert!(matches!(
        recovered.parameters[0].ty.kind,
        TypeKind::Map { .. }
    ));
    assert!(matches!(
        recovered.result.as_ref().map(|result| &result.kind),
        Some(TypeKind::Tuple(elements)) if elements.len() == 2
    ));
}

#[test]
fn nested_interpolation_keeps_inner_strings_braces_comments_and_escapes() {
    let source = r#"
module interpolation.main;
struct Record { text: string; }
fn render(value: i32) -> string {
    return "outer=${Record { text: "inner=${value}", }.text /* } */}, literal=\${value}";
}
"#;
    let tree = parse_nexa(source).expect("small source");
    assert_eq!(tree.reconstructed(), source);
    assert!(tree.errors.is_empty(), "tree errors: {:?}", tree.errors);
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "AST errors: {:?}", ast.errors);
    let DeclarationKind::Function(render) = &ast.declarations[1].kind else {
        panic!("render function");
    };
    let StatementKind::Return(Some(returned)) = &render.body.statements[0].kind else {
        panic!("render returns interpolation");
    };
    let ExpressionKind::Interpolation(parts) = &returned.kind else {
        panic!("outer string is interpolation");
    };
    assert_eq!(parts.len(), 3);
    let InterpolationPart::Expression(expression) = &parts[1] else {
        panic!("middle interpolation expression");
    };
    let ExpressionKind::Member { receiver, member } = &expression.kind else {
        panic!("interpolation keeps the constructed receiver");
    };
    assert_eq!(member.text, "text");
    let ExpressionKind::Construct { fields, .. } = &receiver.kind else {
        panic!("receiver is a struct construction");
    };
    let ExpressionKind::Interpolation(inner) = &fields[0].value.kind else {
        panic!("field contains nested interpolation");
    };
    assert!(matches!(
        inner.as_slice(),
        [
            InterpolationPart::Text { .. },
            InterpolationPart::Expression(_)
        ]
    ));
    let InterpolationPart::Text { cooked, .. } = &parts[2] else {
        panic!("escaped interpolation remains text");
    };
    assert_eq!(cooked, ", literal=${value}");
}

#[test]
fn repository_runtime_examples_have_complete_ast_views() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let roots = [
        workspace.join("examples/combat-runtime"),
        workspace.join("examples/hello-runtime"),
        workspace.join("examples/snake-game/packages"),
        workspace.join("crates/nexa-runtime/fixtures/realm_v5"),
    ];
    let mut sources = Vec::new();
    for root in roots {
        collect_nexa_files(&root, &mut sources);
    }
    assert!(!sources.is_empty(), "expected repository Nexa examples");
    for path in sources {
        let source = std::fs::read_to_string(&path).expect("read Nexa source");
        let tree = parse_nexa(&source).expect("repository sources fit syntax range");
        assert!(
            tree.errors.is_empty(),
            "{} lossless-tree errors: {:?}",
            path.display(),
            tree.errors
        );
        let ast = parse_nexa_ast(&tree);
        assert!(
            ast.errors.is_empty(),
            "{} AST errors: {:?}",
            path.display(),
            ast.errors
        );
    }
}

fn collect_nexa_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.expect("read directory entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_nexa_files(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("nexa") {
            files.push(path);
        }
    }
}
