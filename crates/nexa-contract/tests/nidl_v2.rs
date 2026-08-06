use std::collections::BTreeSet;

use nexa_core::FileId;
use nexa_contract::{
    ABI_DESCRIPTOR_VERSION, CONTRACT_SYNTAX_VERSION, ContractErrorKind, ResolvedTypeKind, abi_descriptor,
    contract_fingerprint, parse, parse_ast_with_file_id,
};

const COMPLETE_CONTRACT: &str = r#"
/// Profile contract documentation.
contract Profile {
    // Ordinary comments are lossless trivia.
    handle Entity;

    /* Field and variant order are ABI-significant. */
    struct Record {
        name: string,
        entity: Entity,
        count: i64,
        ratio: f32,
        weight: f64,
        enabled: bool,
        codepoint: rune,
    }

    enum LoadError {
        Missing,
        Invalid(i32),
        Cancelled,
    }

    host {
        @fuel(8)
        @cancel(return_error)
        @abandon(trap)
        @capability("profile.read")
        async fn load(entity: Entity) -> Result<Record, LoadError>;

        fn store(values: Array<Record>, bytes: Buffer<i32>) -> Option<Entity>;
        fn token(entity: Entity) -> Token<Entity>;
        fn snapshot() -> Snapshot<Record>;
    }

    nexa {
        fn on_event(record: Record) -> Array<i32>;
        async fn refresh(record: Record) -> Result<i32, LoadError>;
    }
}
"#;

#[test]
fn parses_and_validates_the_complete_nidl_v2_surface() {
    let ast = parse_ast_with_file_id(COMPLETE_CONTRACT, FileId(41)).unwrap();
    assert_eq!(ast.contract.name, "Profile");
    assert_eq!(ast.contract.name_span.file, FileId(41));
    assert_eq!(
        ast.contract.docs[0].text,
        " Profile contract documentation."
    );

    let contract = parse(COMPLETE_CONTRACT).unwrap();
    assert_eq!(contract.name, "Profile");
    assert_eq!(contract.handles.len(), 1);
    assert_eq!(contract.structs.len(), 1);
    assert_eq!(contract.enums.len(), 1);
    assert_eq!(contract.host_functions.len(), 4);
    assert_eq!(contract.nexa_functions.len(), 2);
    let load = contract
        .host_functions
        .iter()
        .find(|function| function.name == "load")
        .unwrap();
    assert!(load.is_async);
    assert_eq!(load.fuel_cost, 8);
    assert_eq!(load.capabilities, ["profile.read"]);
    assert!(matches!(
        load.result.as_ref().map(|result| &result.kind),
        Some(ResolvedTypeKind::Result(_, _))
    ));
    assert_eq!(CONTRACT_SYNTAX_VERSION, 2);
    assert_eq!(ABI_DESCRIPTOR_VERSION, 2);
}

#[test]
fn rejects_every_removed_nidl_spelling() {
    let cases = [
        "interface Old {}",
        "contract Old { opaque Entity; }",
        "contract Old { host { sync fn log(); } }",
        "contract Old { host { request fn load(); } }",
        "contract Old { export Run(); }",
        "contract Old { host { fn values() -> array<i32>; } }",
        "contract Old { host { fn values() -> buffer<i32>; } }",
        "contract Old { host { fn values() -> option<i32>; } }",
        "contract Old { host { fn values() -> result<i32, i32>; } }",
        "contract Old { host { fn values() -> token<Entity>; } handle Entity; }",
        "contract Old { host { fn values() -> snapshot<Entity>; } handle Entity; }",
        "contract Old { host { fn values() -> request<i32>; } }",
        "contract Old { host { fn values() -> host_request<i32>; } }",
        "contract Old { host { fn values() -> Request<i32>; } }",
        "contract Old { host { fn values() -> void; } }",
    ];
    for source in cases {
        assert!(
            parse(source).is_err(),
            "removed syntax was accepted: {source}"
        );
    }
}

#[test]
fn validates_names_attributes_layouts_and_source_spans() {
    let cases = [
        (
            "contract bad_name {}",
            ContractErrorKind::InvalidName,
            "bad_name",
        ),
        (
            "contract Duplicate { struct Same {} enum Same {} }",
            ContractErrorKind::Duplicate,
            "Same",
        ),
        (
            "contract Unknown { struct Value { item: Missing, } }",
            ContractErrorKind::UnknownType,
            "Missing",
        ),
        (
            "contract Recursive { struct Node { next: Node, } }",
            ContractErrorKind::RecursiveLayout,
            "Node",
        ),
        (
            "contract Attribute { host { @fuel(0) fn run(); } }",
            ContractErrorKind::InvalidAttribute,
            "@fuel(0)",
        ),
        (
            "contract Attribute { host { @cancel(trap) async fn run(); } }",
            ContractErrorKind::InvalidAttribute,
            "@cancel(trap)",
        ),
        (
            "contract Attribute { host { @abandon(trap) fn run(); } }",
            ContractErrorKind::InvalidAttribute,
            "@abandon(trap)",
        ),
        (
            "contract Naming { host { fn BadName(); } }",
            ContractErrorKind::InvalidName,
            "BadName",
        ),
        (
            "contract Naming { struct Array {} }",
            ContractErrorKind::InvalidName,
            "Array",
        ),
        (
            "contract Attribute { host { @capability(\"scope..read\") fn run(); } }",
            ContractErrorKind::InvalidAttribute,
            "scope..read",
        ),
        (
            "contract Attribute { host { @capability(\"scope:read\") fn run(); } }",
            ContractErrorKind::InvalidAttribute,
            "scope:read",
        ),
        (
            "contract Blocks { host {} host {} }",
            ContractErrorKind::Duplicate,
            "host {}",
        ),
    ];
    for (source, kind, needle) in cases {
        let error = parse(source).expect_err(source);
        assert_eq!(error.kind, kind, "{source}: {error}");
        let span = &source[error.span.start as usize..error.span.end as usize];
        assert!(
            span.contains(needle) || needle.contains(span),
            "{source}: expected `{needle}` around `{span}`"
        );
    }
}

#[test]
fn validates_async_host_return_error_policies_against_the_error_type() {
    let explicit_cancel = "\
        contract Policy {
            enum Fault { Failed, Abandoned, }
            host {
                @cancel(return_error)
                @abandon(return_error)
                async fn run() -> Result<i32, Fault>;
            }
        }";
    let error = parse(explicit_cancel).unwrap_err();
    assert_eq!(error.kind, ContractErrorKind::InvalidType);
    assert_eq!(
        &explicit_cancel[error.span.start as usize..error.span.end as usize],
        "@cancel(return_error)"
    );

    let default_abandon = "\
        contract Policy {
            enum Fault { Failed, Cancelled, }
            host { async fn run() -> Result<i32, Fault>; }
        }";
    let error = parse(default_abandon).unwrap_err();
    assert_eq!(error.kind, ContractErrorKind::InvalidType);
    assert_eq!(
        &default_abandon[error.span.start as usize..error.span.end as usize],
        "Fault"
    );

    parse("contract Policy { host { async fn run() -> Result<i32, i32>; } }").unwrap();
    parse(
        "contract Policy {
            enum Fault { Failed, }
            host {
                @cancel(cancel_task)
                @abandon(trap)
                async fn run() -> Result<i32, Fault>;
            }
        }",
    )
    .unwrap();
}

#[test]
fn stable_ids_are_scoped_by_contract_and_declaration_category() {
    let contract = parse(
        r#"
        contract Stable {
            @stable("shared")
            handle Entity;

            @stable("shared")
            struct Record {}
        }
        "#,
    )
    .unwrap();
    assert_ne!(contract.handles[0].stable_id, contract.structs[0].stable_id);

    let renamed = parse(
        r#"
        @stable("contract")
        contract Renamed {
            @stable("record")
            struct Changed {
                value: i32,
            }
        }
        "#,
    )
    .unwrap();
    let original = parse(
        r#"
        @stable("contract")
        contract Original {
            @stable("record")
            struct Record {
                value: i32,
            }
        }
        "#,
    )
    .unwrap();
    assert_eq!(original.stable_id, renamed.stable_id);
    assert_eq!(original.structs[0].stable_id, renamed.structs[0].stable_id);
    assert_eq!(
        original.structs[0].fields[0].stable_id,
        renamed.structs[0].fields[0].stable_id
    );

    let error = parse(
        r#"
        contract Stable {
            @stable("shared")
            handle Entity;
            @stable("shared")
            handle Actor;
        }
        "#,
    )
    .unwrap_err();
    assert_eq!(error.kind, ContractErrorKind::StableIdCollision);
}

#[test]
fn descriptor_obeys_frozen_order_and_comment_rules() {
    let first = parse(
        r"
        contract Order {
            /// ignored
            struct Pair { first: i32, second: i64, }
            enum Event { Idle, Data(Pair), }
            host { fn read(value: Pair) -> Event; }
            nexa { fn write(value: Event); }
        }
        ",
    )
    .unwrap();
    let top_level_reordered = parse(
        r"
        // Formatting and block order are irrelevant.
        contract Order {
            nexa { fn write(value: Event); }
            host { fn read(value: Pair) -> Event; }
            enum Event { Idle, Data(Pair), }
            struct Pair {
                first: i32,
                second: i64,
            }
        }
        ",
    )
    .unwrap();
    assert_eq!(
        abi_descriptor(&first).bytes,
        abi_descriptor(&top_level_reordered).bytes
    );
    assert_eq!(
        contract_fingerprint(&first),
        contract_fingerprint(&top_level_reordered)
    );

    let fields_reordered = parse(
        r"
        contract Order {
            struct Pair { second: i64, first: i32, }
            enum Event { Idle, Data(Pair), }
            host { fn read(value: Pair) -> Event; }
            nexa { fn write(value: Event); }
        }
        ",
    )
    .unwrap();
    assert_ne!(
        contract_fingerprint(&first),
        contract_fingerprint(&fields_reordered)
    );
}

#[test]
fn async_entrypoint_effect_changes_the_descriptor() {
    let synchronous = parse("contract Effect { nexa { fn run(value: i32) -> i32; } }").unwrap();
    let asynchronous =
        parse("contract Effect { nexa { async fn run(value: i32) -> i32; } }").unwrap();
    assert_ne!(
        contract_fingerprint(&synchronous),
        contract_fingerprint(&asynchronous)
    );
}

#[test]
#[ignore = "scale gate writes a machine-readable report"]
#[allow(clippy::too_many_lines)]
fn m4r1_nidl_mutation_stress() {
    let base = parse(
        "contract Stress { struct Value { item: i32, } \
         host { fn work(value: Value) -> i32; } nexa { fn run(value: i32) -> i32; } }",
    )
    .unwrap();
    let base_fingerprint = contract_fingerprint(&base);
    let mut fingerprints = BTreeSet::new();
    let mut failures = Vec::new();
    let mut categories = std::collections::BTreeMap::<&str, u32>::new();
    let mut mutations = 0_u32;
    for (cycle, suffix) in [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ]
    .into_iter()
    .enumerate()
    {
        let valid_changes = [
            (
                "contract",
                format!(
                    "contract Stress{cycle} {{ struct Value {{ item: i32, }} \
                     host {{ fn work(value: Value) -> i32; }} \
                     nexa {{ fn run(value: i32) -> i32; }} }}"
                ),
            ),
            (
                "host_nexa",
                format!(
                    "contract Stress {{ struct Value {{ item: i32, }} \
                     host {{ fn work_{suffix}(value: Value) -> i32; }} \
                     nexa {{ fn run_{suffix}(value: i32) -> i32; }} }}"
                ),
            ),
            (
                "attributes",
                format!(
                    "contract Stress {{ struct Value {{ item: i32, }} \
                     host {{ @fuel({}) @capability(\"stress.{cycle}\") \
                     fn work(value: Value) -> i32; }} \
                     nexa {{ fn run(value: i32) -> i32; }} }}",
                    cycle + 1
                ),
            ),
            (
                "types",
                format!(
                    "contract Stress {{ struct Value {{ item: i64, }} \
                     host {{ fn work(value: Value) -> i32; }} \
                     nexa {{ fn run(value: i{bits}) -> i32; }} }}",
                    bits = if cycle % 2 == 0 { 32 } else { 64 }
                ),
            ),
        ];
        for (category, source) in valid_changes {
            mutations += 1;
            *categories.entry(category).or_default() += 1;
            match parse(&source) {
                Ok(contract) => {
                    let fingerprint = contract_fingerprint(&contract);
                    if fingerprint == base_fingerprint {
                        failures.push(format!("valid ABI mutation {mutations} did not change"));
                    }
                    fingerprints.insert(fingerprint);
                }
                Err(error) => failures.push(format!("valid mutation {mutations}: {error}")),
            }
        }

        let comment_change = format!(
            "/// comment {cycle}\ncontract Stress {{ struct Value {{ item: i32, }} \
             host {{ /* host {cycle} */ fn work(value: Value) -> i32; }} \
             nexa {{ // nexa\n fn run(value: i32) -> i32; }} }}"
        );
        mutations += 1;
        *categories.entry("comments").or_default() += 1;
        match parse(&comment_change) {
            Ok(contract) if contract_fingerprint(&contract) == base_fingerprint => {}
            Ok(_) => failures.push(format!("comment mutation {mutations} changed ABI")),
            Err(error) => failures.push(format!("comment mutation {mutations}: {error}")),
        }

        let unknown = format!("\n\ncontract Stress {{ struct Value {{ item: Missing{cycle}, }} }}");
        mutations += 1;
        *categories.entry("source_spans").or_default() += 1;
        match parse(&unknown) {
            Err(error) if error.kind == ContractErrorKind::UnknownType => {
                let actual = &unknown[error.span.start as usize..error.span.end as usize];
                if actual != format!("Missing{cycle}") {
                    failures.push(format!(
                        "source span mutation {mutations}: expected Missing{cycle}, got {actual}"
                    ));
                }
            }
            other => failures.push(format!(
                "source span mutation {mutations} returned {other:?}"
            )),
        }

        let invalid_changes = [
            (
                "naming",
                format!("contract bad_name_{cycle} {{}}"),
                ContractErrorKind::InvalidName,
            ),
            (
                "duplicates",
                "contract Stress { struct Same {} enum Same {} }".to_owned(),
                ContractErrorKind::Duplicate,
            ),
            (
                "recursive_layout",
                "contract Stress { struct Node { next: Node, } }".to_owned(),
                ContractErrorKind::RecursiveLayout,
            ),
            (
                "illegal_async",
                "contract Stress { host { async fn load() -> i32; } }".to_owned(),
                ContractErrorKind::InvalidType,
            ),
        ];
        for (category, source, expected) in invalid_changes {
            mutations += 1;
            *categories.entry(category).or_default() += 1;
            match parse(&source) {
                Err(error) if error.kind == expected => {}
                other => failures.push(format!(
                    "invalid mutation {mutations} expected {expected:?}, got {other:?}"
                )),
            }
        }

        let reordered = format!(
            "contract Stress {{ nexa {{ fn run(value: i32) -> i32; }} \
             host {{ fn work(value: Value) -> i32; }} \
             struct Value {{ item: i32, }} /* {cycle} */ }}"
        );
        mutations += 1;
        *categories.entry("host_nexa").or_default() += 1;
        match parse(&reordered) {
            Ok(contract) if contract_fingerprint(&contract) == base_fingerprint => {}
            Ok(_) => failures.push(format!("block order mutation {mutations} changed ABI")),
            Err(error) => failures.push(format!("block order mutation {mutations}: {error}")),
        }

        let async_entrypoint = format!(
            "contract Stress {{ struct Value {{ item: i32, }} \
             host {{ fn work(value: Value) -> i32; }} \
             nexa {{ async fn run_{suffix}(value: i32) -> i32; }} }}"
        );
        mutations += 1;
        *categories.entry("illegal_async").or_default() += 1;
        match parse(&async_entrypoint) {
            Ok(contract) => {
                fingerprints.insert(contract_fingerprint(&contract));
            }
            Err(error) => failures.push(format!("async entrypoint mutation {mutations}: {error}")),
        }
    }
    assert_eq!(mutations, 120);
    assert!(fingerprints.len() >= 30);
    assert!(failures.is_empty(), "{failures:#?}");

    if let Some(path) = std::env::var_os("NEXA_M4R1_NIDL_STRESS_REPORT") {
        let path = std::path::PathBuf::from(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let report = serde_json::json!({
            "schema": 1,
            "status": "PASS",
            "mutations": mutations,
            "categories": categories,
            "failures": failures,
        });
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
