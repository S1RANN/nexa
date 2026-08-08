//! Lossless, error-tolerant syntax infrastructure for Nexa source and Nexa IDL.
//!
//! This crate deliberately stops at syntax. Package paths, module graphs, name
//! resolution and type checking belong to `nexa-analysis`.

/// The source profile of a file. Contract files use the flat `contract Name;`
/// grammar and never join the ordinary source module graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceProfile {
    /// A normal executable source unit (`*.nexa` or extensionless module source).
    Executable,
    /// A Host Contract source unit (`*.contract.nexa`).
    Contract,
}

impl SourceProfile {
    /// The file-extension suffix that marks a source file as a Contract.
    pub const CONTRACT_SUFFIX: &'static str = ".contract.nexa";

    /// Infers the profile from a source path. A `.contract.nexa` suffix selects
    /// [`SourceProfile::Contract`]; everything else is [`SourceProfile::Executable`].
    ///
    /// Suffix matching is exact and case-sensitive, so `snake.contract.nexa` is a
    /// Contract while `snake.Contract.Nexa` or `snake.contract_nexa` are not.
    #[must_use]
    pub fn from_path(path: &str) -> Self {
        if path.ends_with(Self::CONTRACT_SUFFIX) {
            Self::Contract
        } else {
            Self::Executable
        }
    }
}

pub mod ast;
pub mod contract;
mod lexer;
mod text;
mod tree;

pub use contract::{
    ContractAst, ContractAstError, ContractAttribute, ContractAttributeArgument,
    ContractAttributeValue, ContractDocComment, ContractEnumDecl, ContractField, ContractFunction,
    ContractFunctionBlock, ContractFunctionBlockKind, ContractHandleDecl, ContractHeader,
    ContractItem, ContractParameter, ContractStructDecl, ContractTypeRef, ContractVariant,
    parse_contract_ast,
};
pub use lexer::{Lexed, lex_contract, lex_nexa};
pub use text::{
    LineColumn, LineIndex, SourceText, SourceTooLarge, TextEncoding, TextRange, TextSize,
};
pub use tree::{
    AstRoot, CellCompleteness, ContractRoot, Declaration, DeclarationKind, NodeKind, SyntaxError,
    SyntaxErrorKind, SyntaxLanguage, SyntaxNode, SyntaxTree, UseDeclaration, Visibility,
    classify_cell_completeness, parse_contract, parse_nexa,
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
    Impl,
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
    Where,
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
