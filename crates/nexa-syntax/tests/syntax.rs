use nexa_syntax::{
    Keyword, LineColumn, NodeKind, SourceText, SyntaxErrorKind, TextEncoding, TextSize, TokenKind,
    Visibility, lex_nexa, lex_contract, parse_nexa, parse_contract,
};

#[test]
fn nexa_lexer_is_lossless_and_keeps_comment_trivia() {
    let source = "/// score\nuse host::snake; // note\n/* block */ pub fn score() -> i32 { 1 }";
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
    assert!(
        lexed
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::ColonColon)
    );
}

#[test]
fn nidl_v2_comments_strings_and_contract_are_lossless() {
    let source = r#"
/// Snake ABI.
contract Snake;
// Host implementation.
host {
    @capability("profile.read")
    async fn load(id: i32) -> Result<i32, i32>;
}
/* Nexa implementation. */
nexa {
    fn score() -> i32;
}
"#;
    let lexed = lex_contract(source).expect("small source");
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
    let tree = parse_contract(source).expect("small source");
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    assert_eq!(tree.root.children[0].kind, NodeKind::ContractDeclaration);
    assert_eq!(tree.contract().contract_name(), Some("Snake"));
}

#[test]
fn v2_keywords_are_language_specific_and_legacy_words_are_identifiers() {
    let nexa = lex_nexa(
        "fn async return let mut if else while match new await yield defer for in break continue \
         struct enum class use as pub package const true false \
         var module import task immediate migration activation cleanup stateful with",
    )
    .expect("small source");
    let significant = nexa
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .map(|token| token.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        significant[..29],
        [
            Keyword::Fn,
            Keyword::Async,
            Keyword::Return,
            Keyword::Let,
            Keyword::Mut,
            Keyword::If,
            Keyword::Else,
            Keyword::While,
            Keyword::Match,
            Keyword::New,
            Keyword::Await,
            Keyword::Yield,
            Keyword::Defer,
            Keyword::For,
            Keyword::In,
            Keyword::Break,
            Keyword::Continue,
            Keyword::Struct,
            Keyword::Enum,
            Keyword::Class,
            Keyword::Use,
            Keyword::As,
            Keyword::Pub,
            Keyword::Package,
            Keyword::Const,
            Keyword::True,
            Keyword::False,
        ]
        .map(TokenKind::Keyword)
        .into_iter()
        .chain([TokenKind::Identifier, TokenKind::Identifier])
        .collect::<Vec<_>>()
    );
    assert!(
        significant[27..]
            .iter()
            .all(|kind| *kind == TokenKind::Identifier),
        "{significant:?}"
    );

    let nidl = lex_contract(
        "contract host nexa handle async fn struct enum interface opaque sync request export",
    )
    .expect("small source");
    let significant = nidl
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .map(|token| token.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        significant[..8],
        [
            Keyword::Contract,
            Keyword::Host,
            Keyword::Nexa,
            Keyword::Handle,
            Keyword::Async,
            Keyword::Fn,
            Keyword::Struct,
            Keyword::Enum,
        ]
        .map(TokenKind::Keyword)
    );
    assert!(
        significant[8..]
            .iter()
            .all(|kind| *kind == TokenKind::Identifier)
    );
}

#[test]
fn low_level_tree_exposes_uses_declarations_and_script_statements() {
    let source = r"
use package::food::effects;
use snake_common::score as score;
pub const DEFAULT_SCORE: i32 = 10;
fn score() -> i32 { DEFAULT_SCORE }
let mut value = score();
value = value + 1;
";
    let tree = parse_nexa(source).expect("small source");
    assert_eq!(tree.reconstructed(), source);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let root = tree.ast();
    let uses = root
        .uses()
        .map(|use_declaration| {
            (
                use_declaration.path(),
                use_declaration.alias().map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        uses,
        [
            (Some("package::food::effects".into()), None),
            (Some("snake_common::score".into()), Some("score".into())),
        ]
    );
    let declarations = root.declarations().collect::<Vec<_>>();
    assert_eq!(declarations.len(), 2);
    assert_eq!(declarations[0].name(), Some("DEFAULT_SCORE"));
    assert_eq!(declarations[0].visibility(), Visibility::Public);
    assert_eq!(root.top_level_statements().count(), 2);
}

#[test]
fn missing_contract_has_a_stable_nidl_error() {
    let tree = parse_contract("struct Legacy {}").expect("small source");
    assert_eq!(
        tree.errors
            .iter()
            .map(|error| error.kind)
            .collect::<Vec<_>>(),
        [SyntaxErrorKind::MissingContract]
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
}

#[test]
fn utf16_and_scalar_locations_handle_astral_unicode_and_crlf() {
    let source = SourceText::new("a😀b\r\n下一行").expect("small source");
    let index = source.line_index();
    let after_astral = TextSize::new(u32::try_from("a😀".len()).expect("short prefix"));
    assert_eq!(
        index.line_column(after_astral, TextEncoding::Utf16),
        Some(LineColumn { line: 0, column: 3 })
    );
    assert_eq!(
        index.line_column(after_astral, TextEncoding::UnicodeScalar),
        Some(LineColumn { line: 0, column: 2 })
    );
    assert_eq!(
        index.offset(LineColumn { line: 0, column: 2 }, TextEncoding::Utf16),
        None
    );
}

#[test]
fn unknown_unicode_is_preserved_and_reported() {
    let source = "use package::效果;";
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
