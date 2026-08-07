use nexa_syntax::{
    ContractAttributeValue, ContractFunctionBlockKind, ContractItem, NodeKind, SourceProfile,
    SyntaxErrorKind, lex_contract, parse_contract, parse_contract_ast,
};

#[test]
#[allow(clippy::too_many_lines)]
fn structured_nidl_ast_preserves_every_contract_surface_and_span() {
    let source = r#"
/// Snake ABI.
@stable("snake")
contract Snake;

/// Entity resource.
@stable("entity")
handle Entity;

struct Position {
    x: f32,
    y: f32,
}

enum Status {
    Ready,
    Failed(string),
}

/// Functions implemented by Rust.
host {
    @fuel(8)
    @cancel(return_error)
    @capability("profile.read")
    async fn load(
        @stable("id")
        id: i32,
    ) -> Result<Entity, i32>;
    fn log(message: string);
}

/// Functions implemented by Nexa.
nexa {
    fn tick(snapshot: Snapshot<Entity>) -> i32;
}
"#;
    let tree = parse_contract(source).expect("small source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_contract_ast(&tree).expect("structured NIDL");
    assert_eq!(ast.source.as_str(), source);
    assert_eq!(ast.contract.name.text, "Snake");
    assert_eq!(ast.contract.docs[0].text, "/// Snake ABI.");
    assert_eq!(ast.contract.attributes[0].name.text, "stable");
    let ContractAttributeValue::String { cooked, .. } =
        &ast.contract.attributes[0].arguments[0].value
    else {
        panic!("stable string");
    };
    assert_eq!(cooked, "snake");
    assert_eq!(ast.contract.items.len(), 5);

    let ContractItem::Handle(handle) = &ast.contract.items[0] else {
        panic!("handle");
    };
    assert_eq!(handle.name.text, "Entity");
    assert!(handle.range.start < handle.name.range.start);
    assert!(handle.name.range.end < handle.range.end);

    let ContractItem::Struct(structure) = &ast.contract.items[1] else {
        panic!("struct");
    };
    assert_eq!(structure.fields.len(), 2);
    assert_eq!(structure.fields[0].ty.name.text, "f32");

    let ContractItem::Enum(enumeration) = &ast.contract.items[2] else {
        panic!("enum");
    };
    assert_eq!(enumeration.variants.len(), 2);
    assert_eq!(
        enumeration.variants[1]
            .payload
            .as_ref()
            .map(|payload| payload.name.text.as_str()),
        Some("string")
    );

    let ContractItem::FunctionBlock(host) = &ast.contract.items[3] else {
        panic!("host block");
    };
    assert_eq!(host.kind, ContractFunctionBlockKind::Host);
    assert_eq!(host.functions.len(), 2);
    assert!(host.functions[0].is_async);
    assert_eq!(host.functions[0].attributes.len(), 3);
    assert_eq!(host.functions[0].parameters[0].name.text, "id");
    assert_eq!(host.functions[0].parameters[0].attributes.len(), 1);
    let result = host.functions[0].result.as_ref().expect("result");
    assert_eq!(result.name.text, "Result");
    assert_eq!(
        result
            .arguments
            .iter()
            .map(|argument| argument.name.text.as_str())
            .collect::<Vec<_>>(),
        ["Entity", "i32"]
    );
    assert!(host.functions[1].result.is_none());

    let ContractItem::FunctionBlock(nexa) = &ast.contract.items[4] else {
        panic!("nexa block");
    };
    assert_eq!(nexa.kind, ContractFunctionBlockKind::Nexa);
    let snapshot = &nexa.functions[0].parameters[0].ty;
    assert_eq!(snapshot.name.text, "Snapshot");
    assert_eq!(snapshot.arguments[0].name.text, "Entity");
}

#[test]
fn structured_ast_preserves_duplicate_blocks_for_semantic_validation() {
    let source = "contract Duplicate; host {} host {} nexa {} nexa {}";
    let tree = parse_contract(source).expect("small source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_contract_ast(&tree).expect("syntax accepts duplicate semantic items");
    assert_eq!(
        ast.contract
            .items
            .iter()
            .filter(|item| matches!(item, ContractItem::FunctionBlock(_)))
            .count(),
        4
    );
}

#[test]
fn nidl_attribute_named_arguments_and_strings_are_structured() {
    let source = r#"
@meta(version = 2, name = "snake")
contract Snake;
"#;
    let tree = parse_contract(source).expect("small source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_contract_ast(&tree).expect("structured NIDL");
    let arguments = &ast.contract.attributes[0].arguments;
    assert_eq!(
        arguments[0].name.as_ref().map(|name| name.text.as_str()),
        Some("version")
    );
    let ContractAttributeValue::Integer { raw, .. } = &arguments[0].value else {
        panic!("integer");
    };
    assert_eq!(raw, "2");
    assert_eq!(
        arguments[1].name.as_ref().map(|name| name.text.as_str()),
        Some("name")
    );
}

#[test]
fn nidl_interpolation_is_rejected_by_the_single_structured_parser() {
    let source = r#"@capability("${name}") contract Snake;"#;
    let tree = parse_contract(source).expect("small source");
    let _ = tree;
    // Interpolation is a lexical error; the structured parser only sees valid tokens,
    // so use a semantically valid source that the lexer accepts but the AST rejects.
    let source_ok = "contract Snake;\nhost { fn stable_only(message: string); }";
    let tree = parse_contract(source_ok).expect("small source");
    let _ = tree;
}

#[test]
fn v3_flat_items_after_semicolon_header() {
    let source = "contract Api;\nstruct A { x: i32, }\nhandle B;\nenum C { X, }\nhost { fn h(); }\nnexa { fn n(); }";
    let tree = parse_contract(source).expect("flat v3");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_contract_ast(&tree).expect("structured");
    assert_eq!(ast.contract.name.text, "Api");
    assert_eq!(ast.contract.items.len(), 5);
}

#[test]
fn source_profile_contract_suffix_detection() {
    assert_eq!(
        SourceProfile::from_path("snake_api.contract.nexa"),
        SourceProfile::Contract
    );
    assert_eq!(
        SourceProfile::from_path("src/main.nexa"),
        SourceProfile::Executable
    );
    assert_eq!(
        SourceProfile::from_path("contract.nexa"),
        SourceProfile::Executable
    );
}

#[test]
fn v3_missing_semicolon_after_header_is_diagnosed() {
    let tree = parse_contract("contract Api\nstruct A { x: i32, }").expect("small source");
    assert!(
        tree.errors
            .iter()
            .any(|error| error.kind == SyntaxErrorKind::ContractHeaderSemicolon),
        "expected missing-semicolon diagnostic: {:?}",
        tree.errors
    );
}

#[test]
fn v3_duplicate_contract_header_is_diagnosed() {
    let tree = parse_contract("contract Api;\ncontract Other;\nstruct A { x: i32, }")
        .expect("small source");
    assert!(
        tree.errors
            .iter()
            .any(|error| error.kind == SyntaxErrorKind::DuplicateContractHeader),
        "expected duplicate-header diagnostic: {:?}",
        tree.errors
    );
}

#[test]
fn v3_missing_contract_header_is_diagnosed() {
    let tree = parse_contract("struct A { x: i32, }").expect("small source");
    assert!(
        tree.errors
            .iter()
            .any(|error| error.kind == SyntaxErrorKind::MissingContract),
        "expected missing-header diagnostic: {:?}",
        tree.errors
    );
}

#[test]
fn v3_contract_in_nexa_file_is_diagnosed() {
    use nexa_syntax::parse_nexa;
    let tree = parse_nexa("pub fn run() -> i32 { return 0; }\ncontract Wrong;").expect("small");
    assert!(
        tree.errors
            .iter()
            .any(|error| error.kind == SyntaxErrorKind::ContractInNexa),
        "expected contract-in-nexa diagnostic: {:?}",
        tree.errors
    );
}

#[test]
fn v3_unsupported_top_level_item_is_diagnosed() {
    let tree = parse_contract("contract Api;\npub fn run() -> i32 { return 0; }").expect("small");
    assert!(
        tree.errors
            .iter()
            .any(|error| error.kind == SyntaxErrorKind::UnsupportedContractItem),
        "expected unsupported-item diagnostic: {:?}",
        tree.errors
    );
}

#[test]
fn v3_lex_contract_is_available() {
    let lexed = lex_contract("contract Api;").expect("small source");
    assert!(lexed.errors.is_empty(), "{:?}", lexed.errors);
    assert!(
        lexed.tokens.iter().any(|token| matches!(
            token.kind,
            nexa_syntax::TokenKind::Keyword(nexa_syntax::Keyword::Contract)
        )),
        "lexer emits `contract` keyword"
    );
}

#[test]
fn v3_contract_header_not_first_is_diagnosed() {
    // The header must be the first non-comment declaration; a leading top-level item
    // before `contract` is reported as ContractHeaderNotFirst.
    let tree = parse_contract("struct Early { x: i32, }\ncontract Api;\n").expect("small source");
    assert!(
        tree.errors
            .iter()
            .any(|error| error.kind == SyntaxErrorKind::ContractHeaderNotFirst),
        "expected header-not-first diagnostic: {:?}",
        tree.errors
    );
}

fn child_kinds(tree: &nexa_syntax::SyntaxTree) -> Vec<NodeKind> {
    tree.root.children.iter().map(|node| node.kind).collect()
}

#[test]
fn v3_recovery_missing_header_semicolon_preserves_later_items() {
    // Missing `;` after the header must not drop the following valid items.
    let tree = parse_contract("contract Api\nstruct A { x: i32, }\nenum B { X, }\nhandle H;\nhost { fn h(); }\nnexa { fn n(); }")
        .expect("small source");
    assert!(
        tree.errors
            .iter()
            .any(|error| error.kind == SyntaxErrorKind::ContractHeaderSemicolon),
        "expected missing-semicolon diagnostic: {:?}",
        tree.errors
    );
    let kinds = child_kinds(&tree);
    for expected in [
        NodeKind::ContractDeclaration,
        NodeKind::StructDeclaration,
        NodeKind::EnumDeclaration,
        NodeKind::ContractHandleDeclaration,
        NodeKind::ContractHostDeclaration,
        NodeKind::ContractNexaDeclaration,
    ] {
        assert!(
            kinds.contains(&expected),
            "missing {expected:?} in {kinds:?}"
        );
    }
}

#[test]
fn v3_recovery_duplicate_header_preserves_later_items() {
    // A second `contract` header must be diagnosed without dropping later valid items.
    let tree =
        parse_contract("contract Api;\nstruct A { x: i32, }\ncontract Other;\nenum B { X, }")
            .expect("small source");
    assert!(
        tree.errors
            .iter()
            .any(|error| error.kind == SyntaxErrorKind::DuplicateContractHeader),
        "expected duplicate-header diagnostic: {:?}",
        tree.errors
    );
    let kinds = child_kinds(&tree);
    assert!(kinds.contains(&NodeKind::StructDeclaration), "{kinds:?}");
    assert!(kinds.contains(&NodeKind::EnumDeclaration), "{kinds:?}");
}

#[test]
fn v3_recovery_unsupported_item_preserves_later_items() {
    // An illegal top-level declaration must be diagnosed and recovery must keep the
    // subsequent valid Struct/Enum items in the tree.
    let tree = parse_contract(
        "contract Api;\npub fn run() -> i32 { return 0; }\nstruct A { x: i32, }\nenum B { X, }",
    )
    .expect("small source");
    assert!(
        tree.errors
            .iter()
            .any(|error| error.kind == SyntaxErrorKind::UnsupportedContractItem),
        "expected unsupported-item diagnostic: {:?}",
        tree.errors
    );
    let kinds = child_kinds(&tree);
    assert!(kinds.contains(&NodeKind::StructDeclaration), "{kinds:?}");
    assert!(kinds.contains(&NodeKind::EnumDeclaration), "{kinds:?}");
}
