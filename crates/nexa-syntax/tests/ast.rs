use nexa_syntax::ast::{
    AttributeArgumentClassification, DeclarationKind, ExpressionKind, InterpolationPart,
    StatementKind, TypeDeclarationKind, TypeKind, UsePathRootKind, VariantPayload, parse_nexa_ast,
};
use nexa_syntax::parse_nexa;

#[test]
#[allow(clippy::too_many_lines)]
fn complete_v2_ast_covers_uses_async_updates_and_scripts() {
    let source = r#"
use package::food::effects;
use host::snake;
use snake_common::score as score;

@state(version = 1)
class GameState {
    @stable("score")
    mut score: i32,
    name: string,
}

struct Cell {
    x: i32,
    y: i32,
}

enum Food {
    Normal,
    Poison(i32),
    Teleport {
        cell: Cell,
    },
}

@fuel(8)
async fn load_score(id: i32) -> Result<i32, string> {
    let mut values: Array<i32> = Array::new();
    let moved = Cell {
        x: 10,
        ..cell
    };
    let copied = new GameState {
        score: 50,
        ..state
    };
    values[0] = host::snake::load(id).await?.value;
    return moved.x + copied.score + values[0];
}

let mut script_value = 1;
script_value = script_value + 1;
"#;
    let tree = parse_nexa(source).expect("small source");
    assert_eq!(tree.reconstructed(), source);
    assert!(tree.errors.is_empty(), "tree errors: {:?}", tree.errors);

    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "AST errors: {:?}", ast.errors);
    assert_eq!(ast.uses.len(), 3);
    assert_eq!(ast.uses[0].root.kind, UsePathRootKind::Package);
    assert_eq!(ast.uses[0].root.name.text, "package");
    assert_eq!(
        ast.uses[0]
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>(),
        ["food", "effects"]
    );
    assert_eq!(ast.uses[1].root.kind, UsePathRootKind::Host);
    assert_eq!(ast.uses[2].root.kind, UsePathRootKind::Dependency);
    assert_eq!(
        ast.uses[2].alias.as_ref().map(|alias| alias.text.as_str()),
        Some("score")
    );
    assert!(
        ast.uses
            .iter()
            .flat_map(|use_declaration| use_declaration.segments.iter())
            .all(|segment| segment.range.start < segment.range.end)
    );

    let DeclarationKind::Type(state) = &ast.declarations[0].kind else {
        panic!("state class");
    };
    assert_eq!(state.kind, TypeDeclarationKind::Class);
    assert!(state.fields[0].mutable);
    assert!(!state.fields[1].mutable);
    assert_eq!(
        ast.declarations[0].attributes[0].arguments[0].classification,
        AttributeArgumentClassification::Named
    );
    assert_eq!(
        ast.declarations[0].attributes[0].arguments[0]
            .name
            .as_ref()
            .map(|name| name.text.as_str()),
        Some("version")
    );
    assert_eq!(ast.declarations[0].attributes[0].arguments[0].text, "1");

    let DeclarationKind::Type(food) = &ast.declarations[2].kind else {
        panic!("food enum");
    };
    assert!(matches!(&food.variants[0].payload, VariantPayload::Unit));
    assert!(matches!(
        &food.variants[1].payload,
        VariantPayload::Tuple(elements) if elements.len() == 1
    ));
    assert!(matches!(
        &food.variants[2].payload,
        VariantPayload::Struct(fields)
            if fields.len() == 1 && fields[0].name.text == "cell"
    ));

    let DeclarationKind::Function(load) = &ast.declarations[3].kind else {
        panic!("async function");
    };
    assert!(load.is_async);
    assert!(matches!(
        load.result.as_ref().map(|result| &result.kind),
        Some(TypeKind::Result { .. })
    ));
    assert_eq!(
        ast.declarations[3].attributes[0].arguments[0].classification,
        AttributeArgumentClassification::Positional
    );

    let moved = load
        .body
        .statements
        .iter()
        .find_map(|statement| match &statement.kind {
            StatementKind::Bind { name, value, .. } if name.text == "moved" => Some(value),
            _ => None,
        })
        .expect("moved binding");
    let ExpressionKind::Construct { fields, update, .. } = &moved.kind else {
        panic!("struct construction");
    };
    assert_eq!(fields.len(), 1);
    assert!(update.is_some());

    let copied = load
        .body
        .statements
        .iter()
        .find_map(|statement| match &statement.kind {
            StatementKind::Bind { name, value, .. } if name.text == "copied" => Some(value),
            _ => None,
        })
        .expect("copied binding");
    let ExpressionKind::New { fields, update, .. } = &copied.kind else {
        panic!("class construction");
    };
    assert_eq!(fields.len(), 1);
    assert!(update.is_some());

    let awaited = load
        .body
        .statements
        .iter()
        .find_map(|statement| match &statement.kind {
            StatementKind::Assign { value, .. } => Some(value),
            _ => None,
        })
        .expect("assignment");
    let ExpressionKind::Member { receiver, member } = &awaited.kind else {
        panic!("await chain ends in member");
    };
    assert_eq!(member.text, "value");
    let ExpressionKind::Try(receiver) = &receiver.kind else {
        panic!("member receiver is try");
    };
    assert!(matches!(receiver.kind, ExpressionKind::Await { .. }));

    assert_eq!(ast.top_level_statements.len(), 2);
    assert!(matches!(
        ast.top_level_statements[0].kind,
        StatementKind::Bind { mutable: true, .. }
    ));
}

#[test]
fn named_attribute_arguments_classify_unknown_and_duplicates() {
    let source = r"
@state(extra = 0, version = 1, version = 2)
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
            AttributeArgumentClassification::Unknown,
            AttributeArgumentClassification::Named,
            AttributeArgumentClassification::Duplicate,
        ]
    );
}

#[test]
fn nested_interpolation_keeps_inner_strings_and_structs() {
    let source = r#"
struct Record {
    text: string,
}
fn render(value: i32) -> string {
    return "outer=${Record { text: "inner=${value}", }.text}, literal=\${value}";
}
"#;
    let tree = parse_nexa(source).expect("small source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_nexa_ast(&tree);
    assert!(ast.errors.is_empty(), "{:?}", ast.errors);
    let DeclarationKind::Function(render) = &ast.declarations[1].kind else {
        panic!("render function");
    };
    let StatementKind::Return(Some(returned)) = &render.body.statements[0].kind else {
        panic!("return interpolation");
    };
    let ExpressionKind::Interpolation(parts) = &returned.kind else {
        panic!("interpolation");
    };
    assert!(matches!(
        parts.as_slice(),
        [
            InterpolationPart::Text { .. },
            InterpolationPart::Expression(_),
            InterpolationPart::Text { .. }
        ]
    ));
}
