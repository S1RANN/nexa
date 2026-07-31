use nexa_syntax::{
    Keyword, LineColumn, SourceText, SyntaxErrorKind, TextEncoding, TextSize, TokenKind,
    Visibility, lex_nexa, lex_nidl, parse_nexa, parse_nidl,
};

#[test]
fn const_struct_initializer_remains_one_semicolon_terminated_declaration() {
    let source = "module demo.main;\nstruct Record { text: string; }\nconst VALUE: Record = Record { text: \"scale\", };\n";
    let tree = nexa_syntax::parse_nexa(source).expect("small source");
    assert!(tree.errors.is_empty(), "{:#?}", tree.errors);
    assert_eq!(tree.root.children.len(), 3);
    assert_eq!(
        tree.root.children[2].kind,
        nexa_syntax::NodeKind::ConstDeclaration
    );
    assert_eq!(
        tree.root.children[2].range.end.get(),
        u32::try_from(source.len()).unwrap() - 1
    );
}

#[test]
fn nexa_lexer_is_lossless_and_keeps_comment_trivia() {
    let source = "/// score\nmodule snake.score; // note\n/* first /* not nested */ pub fn x() {}";
    let lexed = lex_nexa(source).expect("small source");
    assert_eq!(lexed.reconstructed(), source);
    assert!(lexed.errors.is_empty(), "{:?}", lexed.errors);
    assert!(
        lexed
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::DocComment)
    );
    assert!(
        lexed
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::LineComment)
    );
    assert!(
        lexed
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::BlockComment)
    );
}

#[test]
fn nidl_comments_are_lossless_but_rejected() {
    let source = "interface Host { // forbidden\n sync fn log() -> i32; /* no */ }";
    let lexed = lex_nidl(source).expect("small source");
    assert_eq!(lexed.reconstructed(), source);
    assert_eq!(
        lexed
            .errors
            .iter()
            .filter(|error| error.kind == SyntaxErrorKind::CommentsNotSupported)
            .count(),
        2
    );
    assert_eq!(
        lexed
            .tokens
            .iter()
            .filter(|token| token.kind == TokenKind::InvalidComment)
            .count(),
        2
    );
}

#[test]
fn interpolation_and_escaped_interpolation_have_distinct_tokens() {
    let source = r#""score=${score + format("${nested}")}, literal=\${score}""#;
    let lexed = lex_nexa(source).expect("small source");
    assert_eq!(lexed.reconstructed(), source);
    assert!(lexed.errors.is_empty(), "{:?}", lexed.errors);
    assert_eq!(
        lexed
            .tokens
            .iter()
            .filter(|token| token.kind == TokenKind::InterpolationStart)
            .count(),
        2
    );
    assert_eq!(
        lexed
            .tokens
            .iter()
            .filter(|token| token.kind == TokenKind::InterpolationEnd)
            .count(),
        2
    );
}

#[test]
fn ast_views_recover_module_imports_visibility_attributes_and_names() {
    let source = r#"
        module food.effects;
        import food.types;
        import snake_common.score as score;
        import host as snake;

        /// Stable score policy.
        @stable("score-policy")
        pub(package) const SCORE: i32 = 10;
        @test
        fn score_test() -> bool { true }
        pub task fn on_event() { return; }
    "#;
    let tree = parse_nexa(source).expect("small source");
    assert_eq!(tree.reconstructed(), source);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let root = tree.ast();
    assert_eq!(
        root.module().and_then(|module| module.path()),
        Some("food.effects".into())
    );
    let imports = root
        .imports()
        .map(|import| (import.path(), import.alias().map(str::to_owned)))
        .collect::<Vec<_>>();
    assert_eq!(
        imports,
        vec![
            (Some("food.types".into()), None),
            (Some("snake_common.score".into()), Some("score".into())),
            (Some("host".into()), Some("snake".into())),
        ]
    );
    let declarations = root.declarations().collect::<Vec<_>>();
    assert_eq!(declarations.len(), 3);
    assert_eq!(declarations[0].name(), Some("SCORE"));
    assert_eq!(declarations[0].visibility(), Visibility::Package);
    assert_eq!(declarations[0].attributes(), vec!["stable"]);
    assert_eq!(
        declarations[0].doc_comments(),
        vec!["/// Stable score policy."]
    );
    assert_eq!(declarations[1].attributes(), vec!["test"]);
    assert_eq!(declarations[2].name(), Some("on_event"));
    assert_eq!(declarations[2].visibility(), Visibility::Public);
}

#[test]
fn declaration_views_keep_only_their_own_keyword_attributes_and_docs() {
    let source = r#"
        /// Stateful game state.
        @stateful(2)
        pub class GameState {
            /// Stable score field.
            @stable("score")
            score: i32;
        }

        /// Activation entry.
        @activation
        pub fn activate() -> bool { return true; }
    "#;
    let tree = parse_nexa(source).expect("small source");
    assert!(tree.errors.is_empty(), "{:#?}", tree.errors);
    let declarations = tree.ast().declarations().collect::<Vec<_>>();
    assert_eq!(declarations.len(), 2);
    assert_eq!(declarations[0].name(), Some("GameState"));
    assert_eq!(declarations[0].attributes(), vec!["stateful"]);
    assert_eq!(
        declarations[0].doc_comments(),
        vec!["/// Stateful game state."]
    );
    assert_eq!(declarations[1].name(), Some("activate"));
    assert_eq!(declarations[1].attributes(), vec!["activation"]);
    assert_eq!(
        declarations[1].doc_comments(),
        vec!["/// Activation entry."]
    );
}

#[test]
fn parser_keeps_later_items_after_an_invalid_top_level_token() {
    let source = "module bad\n ???; pub fn recovered() {}";
    let tree = parse_nexa(source).expect("small source");
    assert_eq!(tree.reconstructed(), source);
    assert!(!tree.errors.is_empty());
    assert_eq!(
        tree.ast()
            .declarations()
            .filter_map(|declaration| declaration.name())
            .collect::<Vec<_>>(),
        vec!["recovered"]
    );
}

#[test]
fn utf16_and_scalar_locations_handle_astral_unicode_and_crlf() {
    let source = SourceText::new("a😀b\r\n下一行").expect("small source");
    let index = source.line_index();
    let after_astral = TextSize::new(u32::try_from("a😀".len()).expect("short test prefix"));
    assert_eq!(
        index.line_column(after_astral, TextEncoding::Utf16),
        Some(LineColumn { line: 0, column: 3 })
    );
    assert_eq!(
        index.line_column(after_astral, TextEncoding::UnicodeScalar),
        Some(LineColumn { line: 0, column: 2 })
    );
    assert_eq!(
        index.offset(LineColumn { line: 0, column: 3 }, TextEncoding::Utf16),
        Some(after_astral)
    );
    assert_eq!(
        index.offset(LineColumn { line: 0, column: 2 }, TextEncoding::Utf16),
        None,
        "a UTF-16 column cannot split a surrogate pair"
    );
    assert_eq!(index.line_start(1), Some(TextSize::new(8)));
}

#[test]
fn m4_keywords_are_language_specific() {
    let nexa = lex_nexa("pub package const break continue interface").expect("small source");
    assert_eq!(
        nexa.tokens
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::Keyword(Keyword::Pub),
            TokenKind::Keyword(Keyword::Package),
            TokenKind::Keyword(Keyword::Const),
            TokenKind::Keyword(Keyword::Break),
            TokenKind::Keyword(Keyword::Continue),
            TokenKind::Identifier,
        ]
    );
    let nidl = lex_nidl("interface pub const").expect("small source");
    assert_eq!(
        nidl.tokens
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .map(|token| token.kind)
            .collect::<Vec<_>>(),
        vec![
            TokenKind::Keyword(Keyword::Interface),
            TokenKind::Identifier,
            TokenKind::Identifier,
        ]
    );
}

#[test]
fn nidl_root_exposes_interface_name() {
    let tree = parse_nidl("interface SnakeHost { sync fn score() -> i32; }").expect("small source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    assert_eq!(tree.nidl().interface_name(), Some("SnakeHost"));
}

#[test]
fn unknown_unicode_is_preserved_and_reported() {
    let source = "module food.效果; fn ok() {}";
    let lexed = lex_nexa(source).expect("small source");
    assert_eq!(lexed.reconstructed(), source);
    assert_eq!(
        lexed
            .tokens
            .iter()
            .filter(|token| token.kind == TokenKind::Unknown)
            .count(),
        2
    );
}
