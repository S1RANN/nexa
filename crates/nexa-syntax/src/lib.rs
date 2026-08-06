//! Lossless, error-tolerant syntax infrastructure for Nexa source and Nexa IDL.
//!
//! This crate deliberately stops at syntax. Package paths, module graphs, name
//! resolution and type checking belong to `nexa-analysis`.

pub mod ast;
mod lexer;
pub mod nidl;
mod text;
mod tree;

pub use lexer::{Lexed, lex_nexa, lex_nidl};
pub use nidl::{
    NidlAst, NidlAstError, NidlAttribute, NidlAttributeArgument, NidlAttributeValue, NidlContract,
    NidlContractItem, NidlDocComment, NidlEnum, NidlField, NidlFunction, NidlFunctionBlock,
    NidlFunctionBlockKind, NidlHandle, NidlParameter, NidlStruct, NidlTypeRef, NidlVariant,
    parse_nidl_ast,
};
pub use text::{
    LineColumn, LineIndex, SourceText, SourceTooLarge, TextEncoding, TextRange, TextSize,
};
pub use tree::{
    AstRoot, CellCompleteness, Declaration, DeclarationKind, NidlRoot, NodeKind, SyntaxError,
    SyntaxErrorKind, SyntaxLanguage, SyntaxNode, SyntaxTree, UseDeclaration, Visibility,
    classify_cell_completeness, parse_nexa, parse_nidl,
};

/// A lossless lexical category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Whitespace,
    LineComment,
    DocComment,
    BlockComment,
    Identifier,
    Integer,
    Float,
    Rune,
    StringStart,
    StringText,
    InterpolationStart,
    InterpolationEnd,
    StringEnd,
    Keyword(Keyword),
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    ColonColon,
    Comma,
    Semicolon,
    Arrow,
    FatArrow,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusEqual,
    MinusEqual,
    StarEqual,
    SlashEqual,
    PercentEqual,
    Equal,
    EqualEqual,
    Bang,
    BangEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    AmpAmp,
    PipePipe,
    Question,
    At,
    Dot,
    DotDot,
    Unknown,
}

impl TokenKind {
    #[must_use]
    pub const fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::LineComment | Self::DocComment | Self::BlockComment
        )
    }

    #[must_use]
    pub const fn is_comment(self) -> bool {
        matches!(
            self,
            Self::LineComment | Self::DocComment | Self::BlockComment
        )
    }
}

/// Keywords understood by the syntax layer. Some are only valid in Nexa and
/// some only in NIDL; the language-specific lexer performs that distinction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Keyword {
    Fn,
    Async,
    Return,
    Let,
    Mut,
    If,
    Else,
    While,
    Match,
    New,
    Await,
    Yield,
    Defer,
    For,
    Struct,
    Enum,
    Class,
    Use,
    As,
    In,
    True,
    False,
    Pub,
    Package,
    Const,
    Break,
    Continue,
    Contract,
    Host,
    Nexa,
    Handle,
}

/// A token references its exact byte range in the owning [`SourceText`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token {
    pub kind: TokenKind,
    pub range: TextRange,
}
