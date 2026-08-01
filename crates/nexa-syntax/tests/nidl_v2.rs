use nexa_syntax::{
    NidlAttributeValue, NidlContractItem, NidlFunctionBlockKind, parse_nidl, parse_nidl_ast,
};

#[test]
#[allow(clippy::too_many_lines)]
fn structured_nidl_ast_preserves_every_contract_surface_and_span() {
    let source = r#"
/// Snake ABI.
@stable("snake")
contract Snake {
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
}
"#;
    let tree = parse_nidl(source).expect("small source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ast = parse_nidl_ast(&tree).expect("structured NIDL");
    assert_eq!(ast.source.as_str(), source);
    assert_eq!(ast.contract.name.text, "Snake");
    assert_eq!(ast.contract.docs[0].text, "/// Snake ABI.");
    assert_eq!(ast.contract.attributes[0].name.text, "stable");
    let NidlAttributeValue::String { cooked, .. } = &ast.contract.attributes[0].arguments[0].value
    else {
        panic!("stable string");
    };
    assert_eq!(cooked, "snake");
    assert_eq!(ast.contract.items.len(), 5);

    let NidlContractItem::Handle(handle) = &ast.contract.items[0] else {
        panic!("handle");
    };
    assert_eq!(handle.name.text, "Entity");
    assert!(handle.range.start < handle.name.range.start);
    assert!(handle.name.range.end < handle.range.end);

    let NidlContractItem::Struct(structure) = &ast.contract.items[1] else {
        panic!("struct");
    };
    assert_eq!(structure.fields.len(), 2);
    assert_eq!(structure.fields[0].ty.name.text, "f32");

    let NidlContractItem::Enum(enumeration) = &ast.contract.items[2] else {
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

    let NidlContractItem::FunctionBlock(host) = &ast.contract.items[3] else {
        panic!("host block");
    };
    assert_eq!(host.kind, NidlFunctionBlockKind::Host);
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

    let NidlContractItem::FunctionBlock(nexa) = &ast.contract.items[4] else {
        panic!("nexa block");
    };
    assert_eq!(nexa.kind, NidlFunctionBlockKind::Nexa);
    let snapshot = &nexa.functions[0].parameters[0].ty;
    assert_eq!(snapshot.name.text, "Snapshot");
    assert_eq!(snapshot.arguments[0].name.text, "Entity");
}

#[test]
fn structured_ast_preserves_duplicate_blocks_for_semantic_validation() {
    let source = "contract Duplicate { host {} host {} nexa {} nexa {} }";
    let tree = parse_nidl(source).expect("small source");
    let ast = parse_nidl_ast(&tree).expect("syntax accepts duplicate semantic items");
    assert_eq!(
        ast.contract
            .items
            .iter()
            .filter(|item| matches!(item, NidlContractItem::FunctionBlock(_)))
            .count(),
        4
    );
}

#[test]
fn nidl_attribute_named_arguments_and_strings_are_structured() {
    let source = r#"
@meta(version = 2, name = "snake")
contract Snake {}
"#;
    let tree = parse_nidl(source).expect("small source");
    let ast = parse_nidl_ast(&tree).expect("structured NIDL");
    let arguments = &ast.contract.attributes[0].arguments;
    assert_eq!(
        arguments[0].name.as_ref().map(|name| name.text.as_str()),
        Some("version")
    );
    let NidlAttributeValue::Integer { raw, .. } = &arguments[0].value else {
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
    let source = r#"@capability("${name}") contract Snake {}"#;
    let tree = parse_nidl(source).expect("small source");
    let errors = parse_nidl_ast(&tree).expect_err("NIDL interpolation is forbidden");
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("interpolation is not allowed"))
    );
}
