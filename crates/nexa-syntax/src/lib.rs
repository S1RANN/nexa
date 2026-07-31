//! Lossless, error-tolerant syntax infrastructure for Nexa source and Nexa IDL.
//!
//! This crate deliberately stops at syntax. Package paths, module graphs, name
//! resolution and type checking belong to `nexa-analysis`.

pub mod ast;
mod lexer;
mod text;
mod tree;

pub use lexer::{Lexed, lex_nexa, lex_nidl};
pub use text::{
    LineColumn, LineIndex, SourceText, SourceTooLarge, TextEncoding, TextRange, TextSize,
};
pub use tree::{
    AstRoot, Declaration, DeclarationKind, ImportDeclaration, ModuleDeclaration, NidlRoot,
    NodeKind, SyntaxError, SyntaxErrorKind, SyntaxLanguage, SyntaxNode, SyntaxTree, Visibility,
    parse_nexa, parse_nidl,
};

/// A lossless lexical category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Whitespace,
    LineComment,
    DocComment,
    BlockComment,
    InvalidComment,
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
    Comma,
    Semicolon,
    Arrow,
    FatArrow,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
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
            Self::LineComment | Self::DocComment | Self::BlockComment | Self::InvalidComment
        )
    }
}

/// Keywords understood by the syntax layer. Some are only valid in Nexa and
/// some only in NIDL; the language-specific lexer performs that distinction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Keyword {
    Fn,
    Task,
    Immediate,
    Migration,
    Activation,
    Cleanup,
    Return,
    Let,
    Var,
    If,
    Else,
    While,
    Match,
    With,
    New,
    Await,
    Yield,
    Defer,
    For,
    Struct,
    Enum,
    Class,
    Stateful,
    Module,
    Import,
    As,
    In,
    True,
    False,
    Pub,
    Package,
    Const,
    Break,
    Continue,
    Interface,
    Opaque,
    Sync,
    Request,
    Fuel,
    Policy,
    Export,
}

/// A token references its exact byte range in the owning [`SourceText`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token {
    pub kind: TokenKind,
    pub range: TextRange,
}
