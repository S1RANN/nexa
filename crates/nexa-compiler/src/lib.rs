//! Minimal staged Nexa compiler: lex, parse, resolve/type-check, HIR and verified bytecode.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::{Deref, DerefMut};

use nexa_bytecode::{
    AbandonPolicy, ArrayType, AsyncResultType, BufferType, CancelPolicy, ClassType, EnumType,
    EnumVariant, Function, FunctionEffect, HostCallMode, HostImport, Instruction, MapType, Module,
    ModuleBuilder, RootMap, ScriptExport, Signature, SourceMapEntry, StateField, StateHandleType,
    StateSchema, StateType, StructField as BytecodeStructField, StructType, ValueType,
};
use nexa_core::{FileId, SourceSpan, StableId};
use nexa_idl::{Idl, TypeRef};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
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
    Defer,
    For,
    Struct,
    Enum,
    Class,
    Stateful,
    Module,
    Import,
    In,
    True,
    False,
    Ident(String),
    Integer(i64),
    Float(u64),
    Rune(u32),
    String(String),
    LParen,
    RParen,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Semicolon,
    Arrow,
    Plus,
    Minus,
    Star,
    Slash,
    Equal,
    EqualEqual,
    FatArrow,
    Less,
    Greater,
    Question,
    At,
    Dot,
    DotDot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub offset: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    UnexpectedCharacter {
        offset: usize,
        character: char,
    },
    UnexpectedToken {
        offset: usize,
        expected: &'static str,
    },
    UnexpectedEnd,
    DuplicateName(String),
    UnknownName(String),
    UnknownType(String),
    TypeMismatch,
    InvalidNumericConversion {
        span: SourceSpan,
    },
    CannotInferType,
    NonExhaustiveMatch,
    DuplicateMatchVariant,
    MissingReturn,
    SuspendingDefer,
    DeferCaptureLimit,
    InvalidEffect,
    InvalidReloadMetadata(&'static str),
    TooManyRegisters,
    Verify(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for CompileError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstModule {
    pub name: Option<String>,
    pub imports: Vec<AstImport>,
    pub types: Vec<AstTypeDeclaration>,
    pub functions: Vec<AstFunction>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstImport {
    pub name: String,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstTypeDeclaration {
    pub name: String,
    pub kind: AstTypeKind,
    pub version: u32,
    pub fields: Vec<AstField>,
    pub variants: Vec<AstVariant>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstField {
    pub name: String,
    pub ty: AstType,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstVariant {
    pub name: String,
    pub payload: Option<AstType>,
    pub span: SourceSpan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AstTypeKind {
    Struct,
    Enum,
    Class,
    StatefulClass,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstFunction {
    pub name: String,
    pub is_task: bool,
    pub is_activation: bool,
    pub effect: FunctionEffect,
    pub parameters: Vec<AstParameter>,
    pub result: AstReturnType,
    pub body: Vec<AstStatement>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstParameter {
    pub name: String,
    pub ty: AstType,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstReturnType {
    pub ty: AstType,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AstType {
    I32,
    I64,
    F32,
    F64,
    Bool,
    Rune,
    String,
    Named(String),
    BuiltinGeneric { name: String, arguments: Vec<Self> },
    Spanned { ty: Box<Self>, span: SourceSpan },
}

impl AstType {
    #[must_use]
    pub fn kind(&self) -> &Self {
        match self {
            Self::Spanned { ty, .. } => ty.kind(),
            ty => ty,
        }
    }

    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        match self {
            Self::Spanned { span, .. } => *span,
            _ => SourceSpan::new(FileId(0), 0, 0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AstStatement {
    Bind {
        name: String,
        ty: Option<AstType>,
        value: AstExpression,
    },
    Return(AstExpression),
    Expression(AstExpression),
    If {
        condition: AstExpression,
        then_body: Vec<Self>,
        else_body: Vec<Self>,
    },
    While {
        condition: AstExpression,
        body: Vec<Self>,
    },
    Defer(AstExpression),
    FieldSet {
        value: AstExpression,
        field: String,
        replacement: AstExpression,
    },
    Spanned {
        statement: Box<Self>,
        span: SourceSpan,
    },
}

impl AstStatement {
    #[must_use]
    pub fn kind(&self) -> &Self {
        match self {
            Self::Spanned { statement, .. } => statement.kind(),
            statement => statement,
        }
    }

    fn kind_mut(&mut self) -> &mut Self {
        match self {
            Self::Spanned { statement, .. } => statement.kind_mut(),
            statement => statement,
        }
    }

    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        match self {
            Self::Spanned { span, .. } => *span,
            _ => SourceSpan::new(FileId(0), 0, 0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AstExpression {
    Integer(i64),
    Float(u64),
    Rune(u32),
    String(String),
    Bool(bool),
    Name(String),
    Binary {
        op: AstOperator,
        lhs: Box<Self>,
        rhs: Box<Self>,
    },
    Call {
        function: String,
        arguments: Vec<Self>,
    },
    Await(Box<Self>),
    Constructor {
        type_name: Option<String>,
        variant: String,
        payload: Option<Box<Self>>,
    },
    StructLiteral {
        type_name: String,
        fields: Vec<StructFieldValue>,
    },
    FieldGet {
        value: Box<Self>,
        field: String,
    },
    StructWith {
        value: Box<Self>,
        updates: Vec<StructFieldValue>,
    },
    ClassNew {
        type_name: String,
        fields: Vec<StructFieldValue>,
    },
    ArrayNew {
        element_type: AstType,
    },
    MapNew {
        key_type: AstType,
        value_type: AstType,
    },
    Match {
        value: Box<Self>,
        arms: Vec<MatchArm>,
    },
    Try(Box<Self>),
    Migration(MigrationIntrinsic),
    Spanned {
        expression: Box<Self>,
        span: SourceSpan,
    },
}

impl AstExpression {
    #[must_use]
    pub fn kind(&self) -> &Self {
        match self {
            Self::Spanned { expression, .. } => expression.kind(),
            expression => expression,
        }
    }

    fn kind_mut(&mut self) -> &mut Self {
        match self {
            Self::Spanned { expression, .. } => expression.kind_mut(),
            expression => expression,
        }
    }

    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        match self {
            Self::Spanned { span, .. } => *span,
            _ => SourceSpan::new(FileId(0), 0, 0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchArm {
    pub variant: String,
    pub binding: Option<String>,
    pub value: AstExpression,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructFieldValue {
    pub name: String,
    pub value: AstExpression,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationIntrinsic {
    OldGet {
        stable_id: String,
        ty: AstType,
    },
    OldFieldGet {
        object: Box<AstExpression>,
        owner: String,
        field: String,
        ty: AstType,
    },
    NewCreate {
        stable_id: String,
        ty: AstType,
    },
    NewSet {
        object: Box<AstExpression>,
        owner: String,
        field: String,
        value: Box<AstExpression>,
    },
    Preserve {
        stable_id: String,
    },
    Replace {
        stable_id: String,
        target: Box<AstExpression>,
    },
    Delete {
        stable_id: String,
    },
    Finish,
    Spanned {
        intrinsic: Box<Self>,
        span: SourceSpan,
    },
}

impl MigrationIntrinsic {
    #[must_use]
    pub fn kind(&self) -> &Self {
        match self {
            Self::Spanned { intrinsic, .. } => intrinsic.kind(),
            intrinsic => intrinsic,
        }
    }

    fn kind_mut(&mut self) -> &mut Self {
        match self {
            Self::Spanned { intrinsic, .. } => intrinsic.kind_mut(),
            intrinsic => intrinsic,
        }
    }

    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        match self {
            Self::Spanned { span, .. } => *span,
            _ => SourceSpan::new(FileId(0), 0, 0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AstOperator {
    pub kind: BinaryOp,
    pub span: SourceSpan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StateHandleMethod {
    Resolve,
    IsAlive,
    StableId,
    Generation,
    Equality,
    Hash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringMethod {
    Len,
    ByteLen,
    Equal,
    Concat,
    RuneAt,
    Hash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArrayMethod {
    Len,
    Get,
    Set,
    Push,
    Pop,
    Insert,
    Remove,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MapMethod {
    Len,
    Get,
    Set,
    Remove,
    Contains,
    Clear,
}

fn map_method(function: &str) -> Option<(&str, MapMethod)> {
    let (receiver, method) = function.rsplit_once('.')?;
    let method = match method {
        "len" => MapMethod::Len,
        "get" => MapMethod::Get,
        "set" => MapMethod::Set,
        "remove" => MapMethod::Remove,
        "contains" => MapMethod::Contains,
        "clear" => MapMethod::Clear,
        _ => return None,
    };
    Some((receiver, method))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BufferMethod {
    Len,
    Get,
    Set,
    Slice,
    Copy,
}

fn buffer_method(function: &str) -> Option<(&str, BufferMethod)> {
    let (receiver, method) = function.rsplit_once('.')?;
    let method = match method {
        "len" => BufferMethod::Len,
        "get" => BufferMethod::Get,
        "set" => BufferMethod::Set,
        "slice" => BufferMethod::Slice,
        "copy" => BufferMethod::Copy,
        _ => return None,
    };
    Some((receiver, method))
}

fn array_method(function: &str) -> Option<(&str, ArrayMethod)> {
    let (receiver, method) = function.rsplit_once('.')?;
    let method = match method {
        "len" => ArrayMethod::Len,
        "get" => ArrayMethod::Get,
        "set" => ArrayMethod::Set,
        "push" => ArrayMethod::Push,
        "pop" => ArrayMethod::Pop,
        "insert" => ArrayMethod::Insert,
        "remove" => ArrayMethod::Remove,
        "clear" => ArrayMethod::Clear,
        _ => return None,
    };
    Some((receiver, method))
}

fn string_method(function: &str) -> Option<(&str, StringMethod)> {
    let (receiver, method) = function.rsplit_once('.')?;
    let method = match method {
        "len" | "rune_count" => StringMethod::Len,
        "byte_len" => StringMethod::ByteLen,
        "equals" => StringMethod::Equal,
        "concat" => StringMethod::Concat,
        "rune_at" => StringMethod::RuneAt,
        "hash" => StringMethod::Hash,
        _ => return None,
    };
    Some((receiver, method))
}

fn state_handle_method(function: &str) -> Option<(&str, StateHandleMethod)> {
    let (receiver, method) = function.rsplit_once('.')?;
    let method = match method {
        "resolve" => StateHandleMethod::Resolve,
        "is_alive" => StateHandleMethod::IsAlive,
        "stable_id" => StateHandleMethod::StableId,
        "generation" => StateHandleMethod::Generation,
        "equality" => StateHandleMethod::Equality,
        "hash" => StateHandleMethod::Hash,
        _ => return None,
    };
    Some((receiver, method))
}

fn enum_constructor_name(name: &str) -> Option<(&str, &str)> {
    let (type_name, variant) = name.split_once('.')?;
    if type_name.contains('.')
        || !type_name.chars().next().is_some_and(char::is_uppercase)
        || variant.is_empty()
    {
        return None;
    }
    Some((type_name, variant))
}

#[derive(Clone, Debug)]
pub struct HirModule {
    functions: Vec<HirFunction>,
    host_functions: BTreeMap<String, HostFunction>,
    host_interface_hash: Option<StableId>,
    schema_hash: Option<StableId>,
    state_schema: StateSchema,
    enum_types: Vec<EnumType>,
    enum_variants: BTreeMap<(StableId, String), EnumVariant>,
    struct_types: Vec<StructType>,
    struct_fields: BTreeMap<(StableId, String), BytecodeStructField>,
    class_types: Vec<ClassType>,
    class_fields: BTreeMap<(StableId, String), BytecodeStructField>,
    state_handle_targets: BTreeMap<StableId, ValueType>,
    array_types: Vec<ArrayType>,
    map_types: Vec<MapType>,
    buffer_types: Vec<BufferType>,
    span: SourceSpan,
}

impl HirModule {
    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        self.span
    }

    #[must_use]
    pub fn function_spans(&self) -> impl ExactSizeIterator<Item = SourceSpan> + '_ {
        self.functions.iter().map(|function| function.span)
    }
}

#[derive(Clone, Debug)]
struct HostFunction {
    import: u32,
    signature: Signature,
    metadata: HostImport,
}

#[derive(Clone, Debug)]
struct HirFunction {
    name: String,
    is_activation: bool,
    effect: FunctionEffect,
    signature: Signature,
    body: Vec<AstStatement>,
    locals: BTreeMap<String, (u16, ValueType)>,
    span: SourceSpan,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RegisterPlan {
    local_count: u16,
    expression_temporaries: u16,
    max_call_arguments: u16,
    match_temporaries: u16,
    migration_temporaries: u16,
    total: u16,
}

#[allow(clippy::too_many_lines)]
pub fn lex(source: &str) -> Result<Vec<Token>, CompileError> {
    let mut tokens = Vec::new();
    let mut chars = source.char_indices().peekable();
    while let Some((offset, character)) = chars.next() {
        if character.is_whitespace() {
            continue;
        }
        let mut end = offset.saturating_add(character.len_utf8());
        let kind = match character {
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            ':' => TokenKind::Colon,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semicolon,
            '+' => TokenKind::Plus,
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '=' if chars.peek().is_some_and(|(_, next)| *next == '>') => {
                let (next_offset, next) = chars.next().expect("peeked token exists");
                end = next_offset.saturating_add(next.len_utf8());
                TokenKind::FatArrow
            }
            '=' if chars.peek().is_some_and(|(_, next)| *next == '=') => {
                let (next_offset, next) = chars.next().expect("peeked token exists");
                end = next_offset.saturating_add(next.len_utf8());
                TokenKind::EqualEqual
            }
            '=' => TokenKind::Equal,
            '<' => TokenKind::Less,
            '>' => TokenKind::Greater,
            '?' => TokenKind::Question,
            '@' => TokenKind::At,
            '.' if chars.peek().is_some_and(|(_, next)| *next == '.') => {
                let (next_offset, next) = chars.next().expect("peeked token exists");
                end = next_offset.saturating_add(next.len_utf8());
                TokenKind::DotDot
            }
            '.' => TokenKind::Dot,
            '-' if chars.peek().is_some_and(|(_, next)| *next == '>') => {
                let (next_offset, next) = chars.next().expect("peeked token exists");
                end = next_offset.saturating_add(next.len_utf8());
                TokenKind::Arrow
            }
            '-' => TokenKind::Minus,
            '"' => {
                let mut value = String::new();
                loop {
                    let (value_offset, character) =
                        chars.next().ok_or(CompileError::UnexpectedEnd)?;
                    if character == '"' {
                        end = value_offset + 1;
                        break;
                    }
                    if character == '\\' {
                        let (escape_offset, escape) =
                            chars.next().ok_or(CompileError::UnexpectedEnd)?;
                        value.push(match escape {
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            '\\' => '\\',
                            '"' => '"',
                            character => {
                                return Err(CompileError::UnexpectedCharacter {
                                    offset: escape_offset,
                                    character,
                                });
                            }
                        });
                    } else {
                        value.push(character);
                    }
                }
                TokenKind::String(value)
            }
            '\'' => {
                let (_, value) = chars.next().ok_or(CompileError::UnexpectedEnd)?;
                let value = if value == '\\' {
                    let (escape_offset, escape) =
                        chars.next().ok_or(CompileError::UnexpectedEnd)?;
                    match escape {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '\'' => '\'',
                        character => {
                            return Err(CompileError::UnexpectedCharacter {
                                offset: escape_offset,
                                character,
                            });
                        }
                    }
                } else {
                    value
                };
                let (close_offset, close) = chars.next().ok_or(CompileError::UnexpectedEnd)?;
                if close != '\'' {
                    return Err(CompileError::UnexpectedToken {
                        offset: close_offset,
                        expected: "closing rune quote",
                    });
                }
                end = close_offset + 1;
                TokenKind::Rune(value.into())
            }
            digit if digit.is_ascii_digit() => {
                let mut text = digit.to_string();
                while let Some((next_offset, next)) = chars.peek() {
                    if !next.is_ascii_digit() {
                        break;
                    }
                    text.push(*next);
                    end = next_offset.saturating_add(next.len_utf8());
                    chars.next();
                }
                let decimal = chars.peek().is_some_and(|(_, next)| *next == '.') && {
                    let mut lookahead = chars.clone();
                    lookahead.next();
                    lookahead.peek().is_none_or(|(_, next)| *next != '.')
                };
                if decimal {
                    let (dot_offset, _) = chars.next().expect("peeked decimal point exists");
                    text.push('.');
                    end = dot_offset + 1;
                    let mut digits = 0_usize;
                    while let Some((next_offset, next)) = chars.peek() {
                        if !next.is_ascii_digit() {
                            break;
                        }
                        text.push(*next);
                        end = next_offset.saturating_add(next.len_utf8());
                        chars.next();
                        digits += 1;
                    }
                    if digits == 0 {
                        return Err(CompileError::UnexpectedToken {
                            offset,
                            expected: "floating-point digits",
                        });
                    }
                    TokenKind::Float(
                        text.parse::<f64>()
                            .map_err(|_| CompileError::UnexpectedToken {
                                offset,
                                expected: "floating-point number",
                            })?
                            .to_bits(),
                    )
                } else {
                    TokenKind::Integer(text.parse().map_err(|_| CompileError::UnexpectedToken {
                        offset,
                        expected: "integer",
                    })?)
                }
            }
            first if first == '_' || first.is_ascii_alphabetic() => {
                let mut text = first.to_string();
                while let Some((next_offset, next)) = chars.peek() {
                    if *next != '_' && !next.is_ascii_alphanumeric() {
                        break;
                    }
                    text.push(*next);
                    end = next_offset.saturating_add(next.len_utf8());
                    chars.next();
                }
                match text.as_str() {
                    "fn" => TokenKind::Fn,
                    "task" => TokenKind::Task,
                    "immediate" => TokenKind::Immediate,
                    "migration" => TokenKind::Migration,
                    "activation" => TokenKind::Activation,
                    "cleanup" => TokenKind::Cleanup,
                    "return" => TokenKind::Return,
                    "let" => TokenKind::Let,
                    "var" => TokenKind::Var,
                    "if" => TokenKind::If,
                    "else" => TokenKind::Else,
                    "while" => TokenKind::While,
                    "match" => TokenKind::Match,
                    "with" => TokenKind::With,
                    "new" => TokenKind::New,
                    "await" => TokenKind::Await,
                    "defer" => TokenKind::Defer,
                    "for" => TokenKind::For,
                    "struct" => TokenKind::Struct,
                    "enum" => TokenKind::Enum,
                    "class" => TokenKind::Class,
                    "stateful" => TokenKind::Stateful,
                    "module" => TokenKind::Module,
                    "import" => TokenKind::Import,
                    "in" => TokenKind::In,
                    "true" => TokenKind::True,
                    "false" => TokenKind::False,
                    _ => TokenKind::Ident(text),
                }
            }
            character => {
                return Err(CompileError::UnexpectedCharacter { offset, character });
            }
        };
        tokens.push(Token { kind, offset, end });
    }
    Ok(tokens)
}

pub fn parse(tokens: &[Token]) -> Result<AstModule, CompileError> {
    parse_with_file(tokens, FileId(0))
}

pub fn parse_with_file(tokens: &[Token], file: FileId) -> Result<AstModule, CompileError> {
    Parser {
        tokens,
        cursor: 0,
        file,
    }
    .module()
}

struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    file: FileId,
}

impl Parser<'_> {
    fn module(mut self) -> Result<AstModule, CompileError> {
        let start = self.current_start();
        let name = if self.take(&TokenKind::Module) {
            let name = self.qualified_ident()?;
            self.expect(&TokenKind::Semicolon, ";")?;
            Some(name)
        } else {
            None
        };
        let mut imports = Vec::new();
        while self.at(&TokenKind::Import) {
            let import_start = self.current_start();
            self.cursor += 1;
            let name = self.qualified_ident()?;
            self.expect(&TokenKind::Semicolon, ";")?;
            imports.push(AstImport {
                name,
                span: self.span_from(import_start),
            });
        }
        let mut types = Vec::new();
        let mut functions = Vec::new();
        while self.cursor < self.tokens.len() {
            let is_stateful_type = matches!(
                (
                    self.peek_kind(),
                    self.tokens.get(self.cursor + 1).map(|token| &token.kind)
                ),
                (Some(TokenKind::At), Some(TokenKind::Stateful))
            );
            if is_stateful_type
                || matches!(
                    self.peek_kind(),
                    Some(TokenKind::Struct | TokenKind::Enum | TokenKind::Class)
                )
            {
                types.push(self.type_declaration()?);
            } else {
                functions.push(self.function()?);
            }
        }
        Ok(AstModule {
            name,
            imports,
            types,
            functions,
            span: self.span_from(start),
        })
    }

    fn type_declaration(&mut self) -> Result<AstTypeDeclaration, CompileError> {
        let start = self.current_start();
        let (stateful, version) = if self.take(&TokenKind::At) {
            self.expect(&TokenKind::Stateful, "stateful")?;
            let version = if self.take(&TokenKind::LParen) {
                let version = match self.tokens.get(self.cursor).map(|token| &token.kind) {
                    Some(TokenKind::Integer(version)) => {
                        let version = u32::try_from(*version)
                            .map_err(|_| self.unexpected("positive schema version"))?;
                        self.cursor += 1;
                        version
                    }
                    _ => return Err(self.unexpected("schema version")),
                };
                self.expect(&TokenKind::RParen, ")")?;
                version
            } else {
                1
            };
            (true, version)
        } else {
            (false, 0)
        };
        let kind = if self.take(&TokenKind::Struct) {
            AstTypeKind::Struct
        } else if self.take(&TokenKind::Enum) {
            AstTypeKind::Enum
        } else if self.take(&TokenKind::Class) {
            if stateful {
                AstTypeKind::StatefulClass
            } else {
                AstTypeKind::Class
            }
        } else {
            return Err(self.unexpected("type declaration"));
        };
        let name = self.ident()?;
        self.expect(&TokenKind::LBrace, "{")?;
        let mut fields = Vec::new();
        let mut variants = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            let member_start = self.current_start();
            let member = self.ident()?;
            if kind == AstTypeKind::Enum {
                let payload = if self.take(&TokenKind::LParen) {
                    let payload = self.ty()?;
                    self.expect(&TokenKind::RParen, ")")?;
                    Some(payload)
                } else {
                    None
                };
                variants.push(AstVariant {
                    name: member,
                    payload,
                    span: self.span_from(member_start),
                });
                self.take(&TokenKind::Comma);
            } else {
                self.expect(&TokenKind::Colon, ":")?;
                let ty = self.ty()?;
                self.expect(&TokenKind::Semicolon, ";")?;
                fields.push(AstField {
                    name: member,
                    ty,
                    span: self.span_from(member_start),
                });
            }
        }
        Ok(AstTypeDeclaration {
            name,
            kind,
            version,
            fields,
            variants,
            span: self.span_from(start),
        })
    }

    fn function(&mut self) -> Result<AstFunction, CompileError> {
        let start = self.current_start();
        let is_activation = if self.take(&TokenKind::At) {
            self.expect(&TokenKind::Activation, "activation")?;
            true
        } else {
            false
        };
        let mut effect = if self.take(&TokenKind::Task) {
            FunctionEffect::Task
        } else if self.take(&TokenKind::Immediate) {
            FunctionEffect::Immediate
        } else if self.take(&TokenKind::Migration) {
            FunctionEffect::Migration
        } else if self.take(&TokenKind::Cleanup) {
            FunctionEffect::Cleanup
        } else {
            FunctionEffect::Ordinary
        };
        if is_activation && effect == FunctionEffect::Ordinary {
            effect = FunctionEffect::Immediate;
        }
        let is_task = effect == FunctionEffect::Task;
        self.expect(&TokenKind::Fn, "fn")?;
        let name = self.ident()?;
        self.expect(&TokenKind::LParen, "(")?;
        let mut parameters = Vec::new();
        if !self.at(&TokenKind::RParen) {
            loop {
                let parameter_start = self.current_start();
                let parameter = self.ident()?;
                self.expect(&TokenKind::Colon, ":")?;
                parameters.push(AstParameter {
                    name: parameter,
                    ty: self.ty()?,
                    span: self.span_from(parameter_start),
                });
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen, ")")?;
        self.expect(&TokenKind::Arrow, "->")?;
        let result_start = self.current_start();
        let result = AstReturnType {
            ty: self.ty()?,
            span: self.span_from(result_start),
        };
        let body = self.block()?;
        Ok(AstFunction {
            name,
            is_task,
            is_activation,
            effect,
            parameters,
            result,
            body,
            span: self.span_from(start),
        })
    }

    fn statement(&mut self) -> Result<AstStatement, CompileError> {
        let start = self.current_start();
        if self.take(&TokenKind::Return) {
            let expression = self.expression(0)?;
            self.expect(&TokenKind::Semicolon, ";")?;
            return Ok(self.spanned_statement(start, AstStatement::Return(expression)));
        }
        if self.take(&TokenKind::Let) || self.take(&TokenKind::Var) {
            let name = self.ident()?;
            let ty = if self.take(&TokenKind::Colon) {
                Some(self.ty()?)
            } else {
                None
            };
            self.expect(&TokenKind::Equal, "=")?;
            let value = self.expression(0)?;
            self.expect(&TokenKind::Semicolon, ";")?;
            return Ok(self.spanned_statement(start, AstStatement::Bind { name, ty, value }));
        }
        if self.take(&TokenKind::If) {
            let condition = self.expression(0)?;
            let then_body = self.block()?;
            let else_body = if self.take(&TokenKind::Else) {
                self.block()?
            } else {
                Vec::new()
            };
            return Ok(self.spanned_statement(
                start,
                AstStatement::If {
                    condition,
                    then_body,
                    else_body,
                },
            ));
        }
        if self.take(&TokenKind::For) {
            let variable = self.ident()?;
            self.expect(&TokenKind::In, "in")?;
            let TokenKind::Integer(range_start) = self.next_kind()? else {
                return Err(self.unexpected("static range start"));
            };
            self.expect(&TokenKind::DotDot, "..")?;
            let TokenKind::Integer(range_end) = self.next_kind()? else {
                return Err(self.unexpected("static range end"));
            };
            if range_end < range_start || range_end.saturating_sub(range_start) > 1_024 {
                return Err(CompileError::InvalidEffect);
            }
            let body = self.block()?;
            let mut expanded = Vec::new();
            for value in range_start..range_end {
                let mut iteration = body.clone();
                substitute_name_in_statements(&mut iteration, &variable, value);
                expanded.extend(iteration);
            }
            let condition = AstExpression::Spanned {
                expression: Box::new(AstExpression::Bool(true)),
                span: self.span_from(start),
            };
            return Ok(self.spanned_statement(
                start,
                AstStatement::If {
                    condition,
                    then_body: expanded,
                    else_body: Vec::new(),
                },
            ));
        }
        if self.take(&TokenKind::While) {
            let condition = self.expression(0)?;
            let body = self.block()?;
            return Ok(self.spanned_statement(start, AstStatement::While { condition, body }));
        }
        if self.take(&TokenKind::Defer) {
            let expression = self.expression(0)?;
            self.expect(&TokenKind::Semicolon, ";")?;
            return Ok(self.spanned_statement(start, AstStatement::Defer(expression)));
        }
        let expression = self.expression(0)?;
        if self.take(&TokenKind::Equal) {
            let AstExpression::FieldGet { value, field } = expression.kind() else {
                return Err(self.unexpected("class field assignment"));
            };
            let value = (**value).clone();
            let field = field.clone();
            let replacement = self.expression(0)?;
            self.expect(&TokenKind::Semicolon, ";")?;
            return Ok(self.spanned_statement(
                start,
                AstStatement::FieldSet {
                    value,
                    field,
                    replacement,
                },
            ));
        }
        self.expect(&TokenKind::Semicolon, ";")?;
        Ok(self.spanned_statement(start, AstStatement::Expression(expression)))
    }

    #[allow(clippy::too_many_lines)]
    fn expression(&mut self, minimum_precedence: u8) -> Result<AstExpression, CompileError> {
        let start = self.current_start();
        let mut lhs = match self.next_kind()? {
            TokenKind::Await => AstExpression::Await(Box::new(self.expression(3)?)),
            TokenKind::Match => self.match_expression()?,
            TokenKind::New => {
                if self.take(&TokenKind::Dot) {
                    let name = format!("new.{}", self.ident()?);
                    let type_arguments = if self.take(&TokenKind::Less) {
                        let mut arguments = Vec::new();
                        loop {
                            arguments.push(self.ty()?);
                            if !self.take(&TokenKind::Comma) {
                                break;
                            }
                        }
                        self.expect(&TokenKind::Greater, ">")?;
                        arguments
                    } else {
                        Vec::new()
                    };
                    if !is_migration_intrinsic(&name) {
                        return Err(self.unexpected("migration intrinsic"));
                    }
                    AstExpression::Migration(self.migration_intrinsic(
                        &name,
                        type_arguments,
                        start,
                    )?)
                } else {
                    let type_name = self.ident()?;
                    self.expect(&TokenKind::LBrace, "{")?;
                    AstExpression::ClassNew {
                        type_name,
                        fields: self.struct_field_values()?,
                    }
                }
            }
            TokenKind::Integer(value) => AstExpression::Integer(value),
            TokenKind::Float(bits) => AstExpression::Float(bits),
            TokenKind::Rune(value) => AstExpression::Rune(value),
            TokenKind::String(value) => AstExpression::String(value),
            TokenKind::True => AstExpression::Bool(true),
            TokenKind::False => AstExpression::Bool(false),
            TokenKind::Ident(mut name) => {
                while self.take(&TokenKind::Dot) {
                    let component =
                        if matches!(name.as_str(), "Array" | "Map") && self.take(&TokenKind::New) {
                            "new".to_owned()
                        } else {
                            self.ident()?
                        };
                    name.push('.');
                    name.push_str(&component);
                }
                let type_arguments = if self.take(&TokenKind::Less) {
                    let mut arguments = Vec::new();
                    loop {
                        arguments.push(self.ty()?);
                        if !self.take(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokenKind::Greater, ">")?;
                    arguments
                } else {
                    Vec::new()
                };
                if type_arguments.is_empty()
                    && name.chars().next().is_some_and(char::is_uppercase)
                    && self.take(&TokenKind::LBrace)
                {
                    AstExpression::StructLiteral {
                        type_name: name,
                        fields: self.struct_field_values()?,
                    }
                } else if name == "Array.new" {
                    let element_type = exactly_one_type(type_arguments)?;
                    self.expect(&TokenKind::LParen, "(")?;
                    self.expect(&TokenKind::RParen, ")")?;
                    AstExpression::ArrayNew { element_type }
                } else if name == "Map.new" {
                    let [key_type, value_type] = exactly_two_types(type_arguments)?;
                    self.expect(&TokenKind::LParen, "(")?;
                    self.expect(&TokenKind::RParen, ")")?;
                    AstExpression::MapNew {
                        key_type,
                        value_type,
                    }
                } else if is_migration_intrinsic(&name) {
                    AstExpression::Migration(self.migration_intrinsic(
                        &name,
                        type_arguments,
                        start,
                    )?)
                } else if name == "None" && type_arguments.is_empty() {
                    AstExpression::Constructor {
                        type_name: None,
                        variant: "None".into(),
                        payload: None,
                    }
                } else if matches!(name.as_str(), "Some" | "Ok" | "Err")
                    && type_arguments.is_empty()
                {
                    self.expect(&TokenKind::LParen, "(")?;
                    let payload = self.expression(0)?;
                    self.expect(&TokenKind::RParen, ")")?;
                    AstExpression::Constructor {
                        type_name: None,
                        variant: name,
                        payload: Some(Box::new(payload)),
                    }
                } else if let Some((type_name, variant)) = enum_constructor_name(&name) {
                    if !type_arguments.is_empty() {
                        return Err(self.unexpected("non-generic enum constructor"));
                    }
                    let payload = if self.take(&TokenKind::LParen) {
                        let payload = self.expression(0)?;
                        self.expect(&TokenKind::RParen, ")")?;
                        Some(Box::new(payload))
                    } else {
                        None
                    };
                    AstExpression::Constructor {
                        type_name: Some(type_name.to_owned()),
                        variant: variant.to_owned(),
                        payload,
                    }
                } else if self.take(&TokenKind::LParen) {
                    if !type_arguments.is_empty() {
                        return Err(self.unexpected("non-generic function call"));
                    }
                    let mut arguments = Vec::new();
                    if !self.at(&TokenKind::RParen) {
                        loop {
                            arguments.push(self.expression(0)?);
                            if !self.take(&TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(&TokenKind::RParen, ")")?;
                    AstExpression::Call {
                        function: name,
                        arguments,
                    }
                } else {
                    if !type_arguments.is_empty() {
                        return Err(self.unexpected("generic function call"));
                    }
                    if let Some((receiver, field)) = name.split_once('.')
                        && !receiver.contains('.')
                    {
                        AstExpression::FieldGet {
                            value: Box::new(self.spanned_expression(
                                start,
                                self.previous_end(),
                                AstExpression::Name(receiver.to_owned()),
                            )),
                            field: field.to_owned(),
                        }
                    } else {
                        AstExpression::Name(name)
                    }
                }
            }
            TokenKind::LParen => {
                let expression = self.expression(0)?;
                self.expect(&TokenKind::RParen, ")")?;
                expression
            }
            _ => return Err(self.unexpected("expression")),
        };
        loop {
            if self.take(&TokenKind::With) {
                let inner_end = self.previous_end();
                self.expect(&TokenKind::LBrace, "{")?;
                lhs = AstExpression::StructWith {
                    value: Box::new(self.spanned_expression(start, inner_end, lhs)),
                    updates: self.struct_field_values()?,
                };
                continue;
            }
            if self.at(&TokenKind::Question) {
                let inner_end = self.previous_end();
                self.cursor += 1;
                lhs = AstExpression::Try(Box::new(self.spanned_expression(start, inner_end, lhs)));
                continue;
            }
            let (precedence, op) = match self.peek_kind() {
                Some(TokenKind::EqualEqual) => (0, BinaryOp::Equal),
                Some(TokenKind::Plus) => (1, BinaryOp::Add),
                Some(TokenKind::Minus) => (1, BinaryOp::Subtract),
                Some(TokenKind::Star) => (2, BinaryOp::Multiply),
                Some(TokenKind::Slash) => (2, BinaryOp::Divide),
                _ => break,
            };
            if precedence < minimum_precedence {
                break;
            }
            let operator_span = self.current_token_span();
            let lhs_end = self.previous_end();
            self.cursor += 1;
            let rhs = self.expression(precedence + 1)?;
            lhs = AstExpression::Binary {
                op: AstOperator {
                    kind: op,
                    span: operator_span,
                },
                lhs: Box::new(self.spanned_expression(start, lhs_end, lhs)),
                rhs: Box::new(rhs),
            };
        }
        Ok(AstExpression::Spanned {
            expression: Box::new(lhs),
            span: self.span_from(start),
        })
    }

    fn match_expression(&mut self) -> Result<AstExpression, CompileError> {
        let value = self.expression(0)?;
        self.expect(&TokenKind::LBrace, "{")?;
        let mut arms = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            let arm_start = self.current_start();
            let variant = self.ident()?;
            let binding = if self.take(&TokenKind::LParen) {
                let binding = self.ident()?;
                self.expect(&TokenKind::RParen, ")")?;
                Some(binding)
            } else {
                None
            };
            self.expect(&TokenKind::FatArrow, "=>")?;
            let arm_value = self.expression(0)?;
            arms.push(MatchArm {
                variant,
                binding,
                value: arm_value,
                span: self.span_from(arm_start),
            });
            if !self.take(&TokenKind::Comma) && !self.at(&TokenKind::RBrace) {
                return Err(self.unexpected(", or }"));
            }
        }
        Ok(AstExpression::Match {
            value: Box::new(value),
            arms,
        })
    }

    fn struct_field_values(&mut self) -> Result<Vec<StructFieldValue>, CompileError> {
        let mut fields = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            let field_start = self.current_start();
            let name = self.ident()?;
            self.expect(&TokenKind::Colon, ":")?;
            let value = self.expression(0)?;
            fields.push(StructFieldValue {
                name,
                value,
                span: self.span_from(field_start),
            });
            if !self.take(&TokenKind::Comma) && !self.at(&TokenKind::RBrace) {
                return Err(self.unexpected(", or }"));
            }
        }
        Ok(fields)
    }

    fn migration_intrinsic(
        &mut self,
        name: &str,
        type_arguments: Vec<AstType>,
        start: usize,
    ) -> Result<MigrationIntrinsic, CompileError> {
        self.expect(&TokenKind::LParen, "(")?;
        let intrinsic = match name {
            "old.get" => MigrationIntrinsic::OldGet {
                ty: exactly_one_type(type_arguments)?,
                stable_id: self.ident()?,
            },
            "old.field" => {
                let object = self.expression(0)?;
                self.expect(&TokenKind::Comma, ",")?;
                let (owner, field) = split_field_name(&self.qualified_ident()?)?;
                MigrationIntrinsic::OldFieldGet {
                    object: Box::new(object),
                    owner,
                    field,
                    ty: exactly_one_type(type_arguments)?,
                }
            }
            "new.create" => MigrationIntrinsic::NewCreate {
                ty: exactly_one_type(type_arguments)?,
                stable_id: self.ident()?,
            },
            "new.set" => {
                if !type_arguments.is_empty() {
                    return Err(CompileError::TypeMismatch);
                }
                let object = self.expression(0)?;
                self.expect(&TokenKind::Comma, ",")?;
                let (owner, field) = split_field_name(&self.qualified_ident()?)?;
                self.expect(&TokenKind::Comma, ",")?;
                let value = self.expression(0)?;
                MigrationIntrinsic::NewSet {
                    object: Box::new(object),
                    owner,
                    field,
                    value: Box::new(value),
                }
            }
            "preserve" => MigrationIntrinsic::Preserve {
                stable_id: self.ident()?,
            },
            "replace" => {
                let stable_id = self.ident()?;
                self.expect(&TokenKind::Comma, ",")?;
                MigrationIntrinsic::Replace {
                    stable_id,
                    target: Box::new(self.expression(0)?),
                }
            }
            "delete" => MigrationIntrinsic::Delete {
                stable_id: self.ident()?,
            },
            "finish_migration" => {
                if !type_arguments.is_empty() {
                    return Err(CompileError::TypeMismatch);
                }
                MigrationIntrinsic::Finish
            }
            _ => unreachable!(),
        };
        self.expect(&TokenKind::RParen, ")")?;
        Ok(MigrationIntrinsic::Spanned {
            intrinsic: Box::new(intrinsic),
            span: self.span_from(start),
        })
    }

    fn block(&mut self) -> Result<Vec<AstStatement>, CompileError> {
        self.expect(&TokenKind::LBrace, "{")?;
        let mut body = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            body.push(self.statement()?);
        }
        Ok(body)
    }

    fn ty(&mut self) -> Result<AstType, CompileError> {
        let start = self.current_start();
        let name = self.ident()?;
        let base = match name.as_str() {
            "i32" => AstType::I32,
            "i64" => AstType::I64,
            "f32" => AstType::F32,
            "f64" => AstType::F64,
            "bool" => AstType::Bool,
            "rune" => AstType::Rune,
            "string" => AstType::String,
            named => AstType::Named(named.to_owned()),
        };
        if self.take(&TokenKind::Less) {
            let mut arguments = Vec::new();
            loop {
                arguments.push(self.ty()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::Greater, ">")?;
            Ok(AstType::Spanned {
                ty: Box::new(AstType::BuiltinGeneric { name, arguments }),
                span: self.span_from(start),
            })
        } else {
            Ok(AstType::Spanned {
                ty: Box::new(base),
                span: self.span_from(start),
            })
        }
    }

    fn ident(&mut self) -> Result<String, CompileError> {
        match self.next_kind()? {
            TokenKind::Ident(name) => Ok(name),
            _ => Err(self.unexpected("identifier")),
        }
    }

    fn qualified_ident(&mut self) -> Result<String, CompileError> {
        let mut name = self.ident()?;
        while self.take(&TokenKind::Dot) {
            name.push('.');
            name.push_str(&self.ident()?);
        }
        Ok(name)
    }

    fn next_kind(&mut self) -> Result<TokenKind, CompileError> {
        let token = self
            .tokens
            .get(self.cursor)
            .ok_or(CompileError::UnexpectedEnd)?;
        self.cursor += 1;
        Ok(token.kind.clone())
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.tokens.get(self.cursor).map(|token| &token.kind)
    }

    fn at(&self, expected: &TokenKind) -> bool {
        self.peek_kind() == Some(expected)
    }

    fn take(&mut self, expected: &TokenKind) -> bool {
        if self.at(expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &TokenKind, name: &'static str) -> Result<(), CompileError> {
        if self.take(expected) {
            Ok(())
        } else {
            Err(self.unexpected(name))
        }
    }

    fn unexpected(&self, expected: &'static str) -> CompileError {
        self.tokens
            .get(self.cursor)
            .map_or(CompileError::UnexpectedEnd, |token| {
                CompileError::UnexpectedToken {
                    offset: token.offset,
                    expected,
                }
            })
    }

    fn current_start(&self) -> usize {
        self.tokens
            .get(self.cursor)
            .map_or_else(|| self.previous_end(), |token| token.offset)
    }

    fn previous_end(&self) -> usize {
        self.cursor
            .checked_sub(1)
            .and_then(|index| self.tokens.get(index))
            .map_or(0, |token| token.end)
    }

    fn span_from(&self, start: usize) -> SourceSpan {
        SourceSpan::new(
            self.file,
            u32::try_from(start).unwrap_or(u32::MAX),
            u32::try_from(self.previous_end()).unwrap_or(u32::MAX),
        )
    }

    fn current_token_span(&self) -> SourceSpan {
        self.tokens.get(self.cursor).map_or_else(
            || self.span_from(self.previous_end()),
            |token| {
                SourceSpan::new(
                    self.file,
                    u32::try_from(token.offset).unwrap_or(u32::MAX),
                    u32::try_from(token.end).unwrap_or(u32::MAX),
                )
            },
        )
    }

    fn spanned_statement(&self, start: usize, statement: AstStatement) -> AstStatement {
        AstStatement::Spanned {
            statement: Box::new(statement),
            span: self.span_from(start),
        }
    }

    fn spanned_expression(
        &self,
        start: usize,
        end: usize,
        expression: AstExpression,
    ) -> AstExpression {
        AstExpression::Spanned {
            expression: Box::new(expression),
            span: SourceSpan::new(
                self.file,
                u32::try_from(start).unwrap_or(u32::MAX),
                u32::try_from(end).unwrap_or(u32::MAX),
            ),
        }
    }
}

fn is_migration_intrinsic(name: &str) -> bool {
    matches!(
        name,
        "old.get"
            | "old.field"
            | "new.create"
            | "new.set"
            | "preserve"
            | "replace"
            | "delete"
            | "finish_migration"
    )
}

fn exactly_one_type(mut arguments: Vec<AstType>) -> Result<AstType, CompileError> {
    if arguments.len() != 1 {
        return Err(CompileError::TypeMismatch);
    }
    Ok(arguments.pop().expect("length was checked"))
}

fn exactly_two_types(arguments: Vec<AstType>) -> Result<[AstType; 2], CompileError> {
    arguments.try_into().map_err(|_| CompileError::TypeMismatch)
}

fn split_field_name(name: &str) -> Result<(String, String), CompileError> {
    name.rsplit_once('.')
        .map(|(owner, field)| (owner.to_owned(), field.to_owned()))
        .ok_or(CompileError::TypeMismatch)
}

fn migration_expressions(intrinsic: &MigrationIntrinsic) -> Vec<&AstExpression> {
    match intrinsic.kind() {
        MigrationIntrinsic::OldFieldGet { object, .. } => vec![object],
        MigrationIntrinsic::NewSet { object, value, .. } => vec![object, value],
        MigrationIntrinsic::Replace { target, .. } => vec![target],
        MigrationIntrinsic::OldGet { .. }
        | MigrationIntrinsic::NewCreate { .. }
        | MigrationIntrinsic::Preserve { .. }
        | MigrationIntrinsic::Delete { .. }
        | MigrationIntrinsic::Finish => Vec::new(),
        MigrationIntrinsic::Spanned { .. } => unreachable!("kind strips spans"),
    }
}

fn migration_expressions_mut(intrinsic: &mut MigrationIntrinsic) -> Vec<&mut AstExpression> {
    match intrinsic.kind_mut() {
        MigrationIntrinsic::OldFieldGet { object, .. } => vec![object],
        MigrationIntrinsic::NewSet { object, value, .. } => vec![object, value],
        MigrationIntrinsic::Replace { target, .. } => vec![target],
        MigrationIntrinsic::OldGet { .. }
        | MigrationIntrinsic::NewCreate { .. }
        | MigrationIntrinsic::Preserve { .. }
        | MigrationIntrinsic::Delete { .. }
        | MigrationIntrinsic::Finish => Vec::new(),
        MigrationIntrinsic::Spanned { .. } => unreachable!("kind_mut strips spans"),
    }
}

fn substitute_name_in_statements(statements: &mut [AstStatement], name: &str, value: i64) {
    for statement in statements {
        match statement.kind_mut() {
            AstStatement::Bind {
                value: expression, ..
            }
            | AstStatement::Return(expression)
            | AstStatement::Expression(expression)
            | AstStatement::Defer(expression) => substitute_name(expression, name, value),
            AstStatement::FieldSet {
                value: object,
                replacement,
                ..
            } => {
                substitute_name(object, name, value);
                substitute_name(replacement, name, value);
            }
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                substitute_name(condition, name, value);
                substitute_name_in_statements(then_body, name, value);
                substitute_name_in_statements(else_body, name, value);
            }
            AstStatement::While { condition, body } => {
                substitute_name(condition, name, value);
                substitute_name_in_statements(body, name, value);
            }
            AstStatement::Spanned { .. } => unreachable!("kind_mut strips spans"),
        }
    }
}

fn substitute_name(expression: &mut AstExpression, name: &str, value: i64) {
    let kind = expression.kind_mut();
    match kind {
        AstExpression::Name(current) if current == name => {
            *kind = AstExpression::Integer(value);
        }
        AstExpression::Binary { lhs, rhs, .. } => {
            substitute_name(lhs, name, value);
            substitute_name(rhs, name, value);
        }
        AstExpression::Call { arguments, .. } => {
            for argument in arguments {
                substitute_name(argument, name, value);
            }
        }
        AstExpression::Await(expression) | AstExpression::Try(expression) => {
            substitute_name(expression, name, value);
        }
        AstExpression::Constructor { payload, .. } => {
            if let Some(payload) = payload {
                substitute_name(payload, name, value);
            }
        }
        AstExpression::StructLiteral { fields, .. } | AstExpression::ClassNew { fields, .. } => {
            for field in fields {
                substitute_name(&mut field.value, name, value);
            }
        }
        AstExpression::FieldGet {
            value: field_value, ..
        } => substitute_name(field_value, name, value),
        AstExpression::StructWith {
            value: struct_value,
            updates,
        } => {
            substitute_name(struct_value, name, value);
            for field in updates {
                substitute_name(&mut field.value, name, value);
            }
        }
        AstExpression::Match {
            value: matched,
            arms,
        } => {
            substitute_name(matched, name, value);
            for arm in arms {
                substitute_name(&mut arm.value, name, value);
            }
        }
        AstExpression::Migration(intrinsic) => {
            for expression in migration_expressions_mut(intrinsic) {
                substitute_name(expression, name, value);
            }
        }
        AstExpression::Integer(_)
        | AstExpression::Float(_)
        | AstExpression::Rune(_)
        | AstExpression::String(_)
        | AstExpression::Bool(_)
        | AstExpression::ArrayNew { .. }
        | AstExpression::MapNew { .. }
        | AstExpression::Name(_) => {}
        AstExpression::Spanned { .. } => unreachable!("kind_mut strips spans"),
    }
}

pub fn resolve_and_typecheck(ast: AstModule) -> Result<HirModule, CompileError> {
    resolve_and_typecheck_with_hosts(ast, BTreeMap::new(), None, None)
}

fn builtin_variant_id(name: &str) -> StableId {
    match name {
        "None" => StableId::from_parts(&["Option", "::None"]),
        "Some" => StableId::from_parts(&["Option", "::Some"]),
        "Ok" => StableId::from_parts(&["Result", "::Ok"]),
        "Err" => StableId::from_parts(&["Result", "::Err"]),
        "WrongDomain" | "Missing" | "StaleGeneration" | "GenerationExhausted" => {
            StableId::from_parts(&["StateHandleError", "::", name])
        }
        _ => StableId::from_name(name),
    }
}

fn collect_builtin_enum_types(ast: &AstModule, enum_types: &mut Vec<EnumType>) {
    let mut add_type = |ty: &AstType| collect_builtin_enum_type(ty, enum_types);
    for declaration in &ast.types {
        for field in &declaration.fields {
            add_type(&field.ty);
        }
        for variant in &declaration.variants {
            if let Some(payload) = &variant.payload {
                add_type(payload);
            }
        }
    }
    for function in &ast.functions {
        for parameter in &function.parameters {
            add_type(&parameter.ty);
        }
        add_type(&function.result.ty);
        collect_statement_builtin_types(&function.body, &mut add_type);
    }
}

fn collect_statement_builtin_types(
    statements: &[AstStatement],
    add_type: &mut impl FnMut(&AstType),
) {
    for statement in statements {
        match statement.kind() {
            AstStatement::Bind { ty: Some(ty), .. } => add_type(ty),
            AstStatement::If {
                then_body,
                else_body,
                ..
            } => {
                collect_statement_builtin_types(then_body, add_type);
                collect_statement_builtin_types(else_body, add_type);
            }
            AstStatement::While { body, .. } => {
                collect_statement_builtin_types(body, add_type);
            }
            AstStatement::Bind { ty: None, .. }
            | AstStatement::Return(_)
            | AstStatement::Expression(_)
            | AstStatement::Defer(_)
            | AstStatement::FieldSet { .. } => {}
            AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
        }
    }
}

fn collect_builtin_enum_type(ty: &AstType, enum_types: &mut Vec<EnumType>) {
    let AstType::BuiltinGeneric { name, arguments } = ty.kind() else {
        return;
    };
    for argument in arguments {
        collect_builtin_enum_type(argument, enum_types);
    }
    let enum_type = match name.as_str() {
        "Option" if arguments.len() == 1 => {
            Some(nexa_bytecode::option_type(lower_type(&arguments[0])))
        }
        "Result" if arguments.len() == 2 => Some(nexa_bytecode::result_type(
            lower_type(&arguments[0]),
            lower_type(&arguments[1]),
        )),
        _ => None,
    };
    if let Some(enum_type) = enum_type
        && !enum_types
            .iter()
            .any(|candidate| candidate.type_id == enum_type.type_id)
    {
        enum_types.push(enum_type);
    }
}

fn collect_state_handle_targets(ast: &AstModule) -> BTreeMap<StableId, ValueType> {
    fn collect(ty: &AstType, targets: &mut BTreeMap<StableId, ValueType>) {
        let AstType::BuiltinGeneric { name, arguments } = ty.kind() else {
            return;
        };
        for argument in arguments {
            collect(argument, targets);
        }
        if name == "StateHandle" && arguments.len() == 1 {
            let target = lower_type(&arguments[0]);
            targets.insert(nexa_bytecode::state_handle_type(target), target);
        }
    }

    let mut targets = BTreeMap::new();
    for declaration in &ast.types {
        for field in &declaration.fields {
            collect(&field.ty, &mut targets);
        }
        for variant in &declaration.variants {
            if let Some(payload) = &variant.payload {
                collect(payload, &mut targets);
            }
        }
    }
    for function in &ast.functions {
        for parameter in &function.parameters {
            collect(&parameter.ty, &mut targets);
        }
        collect(&function.result.ty, &mut targets);
        collect_statement_types(&function.body, &mut |ty| collect(ty, &mut targets));
    }
    targets
}

#[allow(clippy::too_many_lines)]
fn collect_collection_types(ast: &AstModule) -> (Vec<ArrayType>, Vec<MapType>) {
    fn collect_type(
        ty: &AstType,
        arrays: &mut BTreeMap<StableId, ArrayType>,
        maps: &mut BTreeMap<StableId, MapType>,
    ) {
        let AstType::BuiltinGeneric { name, arguments } = ty.kind() else {
            return;
        };
        for argument in arguments {
            collect_type(argument, arrays, maps);
        }
        if name == "Array" && arguments.len() == 1 {
            let array = ArrayType::new(lower_type(&arguments[0]));
            arrays.insert(array.type_id, array);
        } else if name == "Map" && arguments.len() == 2 {
            let map = MapType::new(lower_type(&arguments[0]), lower_type(&arguments[1]));
            maps.insert(map.type_id, map);
        }
    }
    fn collect_expression(
        expression: &AstExpression,
        arrays: &mut BTreeMap<StableId, ArrayType>,
        maps: &mut BTreeMap<StableId, MapType>,
    ) {
        match expression.kind() {
            AstExpression::ArrayNew { element_type } => collect_type(element_type, arrays, maps),
            AstExpression::MapNew {
                key_type,
                value_type,
            } => {
                collect_type(key_type, arrays, maps);
                collect_type(value_type, arrays, maps);
                let map = MapType::new(lower_type(key_type), lower_type(value_type));
                maps.insert(map.type_id, map);
            }
            AstExpression::Binary { lhs, rhs, .. } => {
                collect_expression(lhs, arrays, maps);
                collect_expression(rhs, arrays, maps);
            }
            AstExpression::Call { arguments, .. } => {
                for argument in arguments {
                    collect_expression(argument, arrays, maps);
                }
            }
            AstExpression::Await(expression) | AstExpression::Try(expression) => {
                collect_expression(expression, arrays, maps);
            }
            AstExpression::Constructor { payload, .. } => {
                if let Some(payload) = payload {
                    collect_expression(payload, arrays, maps);
                }
            }
            AstExpression::StructLiteral { fields, .. }
            | AstExpression::ClassNew { fields, .. } => {
                for field in fields {
                    collect_expression(&field.value, arrays, maps);
                }
            }
            AstExpression::FieldGet { value, .. } => collect_expression(value, arrays, maps),
            AstExpression::StructWith { value, updates } => {
                collect_expression(value, arrays, maps);
                for update in updates {
                    collect_expression(&update.value, arrays, maps);
                }
            }
            AstExpression::Match { value, arms } => {
                collect_expression(value, arrays, maps);
                for arm in arms {
                    collect_expression(&arm.value, arrays, maps);
                }
            }
            AstExpression::Migration(intrinsic) => {
                match intrinsic.kind() {
                    MigrationIntrinsic::OldGet { ty, .. }
                    | MigrationIntrinsic::OldFieldGet { ty, .. }
                    | MigrationIntrinsic::NewCreate { ty, .. } => collect_type(ty, arrays, maps),
                    MigrationIntrinsic::NewSet { .. }
                    | MigrationIntrinsic::Preserve { .. }
                    | MigrationIntrinsic::Replace { .. }
                    | MigrationIntrinsic::Delete { .. }
                    | MigrationIntrinsic::Finish => {}
                    MigrationIntrinsic::Spanned { .. } => unreachable!("kind strips spans"),
                }
                for expression in migration_expressions(intrinsic) {
                    collect_expression(expression, arrays, maps);
                }
            }
            AstExpression::Integer(_)
            | AstExpression::Float(_)
            | AstExpression::Rune(_)
            | AstExpression::String(_)
            | AstExpression::Bool(_)
            | AstExpression::Name(_) => {}
            AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
        }
    }
    fn collect_statements(
        statements: &[AstStatement],
        arrays: &mut BTreeMap<StableId, ArrayType>,
        maps: &mut BTreeMap<StableId, MapType>,
    ) {
        for statement in statements {
            match statement.kind() {
                AstStatement::Bind { ty, value, .. } => {
                    if let Some(ty) = ty {
                        collect_type(ty, arrays, maps);
                    }
                    collect_expression(value, arrays, maps);
                }
                AstStatement::Return(value)
                | AstStatement::Expression(value)
                | AstStatement::Defer(value) => collect_expression(value, arrays, maps),
                AstStatement::FieldSet {
                    value, replacement, ..
                } => {
                    collect_expression(value, arrays, maps);
                    collect_expression(replacement, arrays, maps);
                }
                AstStatement::If {
                    condition,
                    then_body,
                    else_body,
                } => {
                    collect_expression(condition, arrays, maps);
                    collect_statements(then_body, arrays, maps);
                    collect_statements(else_body, arrays, maps);
                }
                AstStatement::While { condition, body } => {
                    collect_expression(condition, arrays, maps);
                    collect_statements(body, arrays, maps);
                }
                AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
            }
        }
    }

    let mut arrays = BTreeMap::new();
    let mut maps = BTreeMap::new();
    for declaration in &ast.types {
        for field in &declaration.fields {
            collect_type(&field.ty, &mut arrays, &mut maps);
        }
        for variant in &declaration.variants {
            if let Some(payload) = &variant.payload {
                collect_type(payload, &mut arrays, &mut maps);
            }
        }
    }
    for function in &ast.functions {
        for parameter in &function.parameters {
            collect_type(&parameter.ty, &mut arrays, &mut maps);
        }
        collect_type(&function.result.ty, &mut arrays, &mut maps);
        collect_statements(&function.body, &mut arrays, &mut maps);
    }
    (arrays.into_values().collect(), maps.into_values().collect())
}

fn collect_buffer_types(ast: &AstModule) -> Vec<BufferType> {
    fn collect(ty: &AstType, buffers: &mut BTreeMap<StableId, BufferType>) {
        let AstType::BuiltinGeneric { name, arguments } = ty.kind() else {
            return;
        };
        for argument in arguments {
            collect(argument, buffers);
        }
        if name == "Buffer" && arguments.len() == 1 {
            let buffer = BufferType::new(lower_type(&arguments[0]));
            buffers.insert(buffer.type_id, buffer);
        }
    }

    let mut buffers = BTreeMap::new();
    for declaration in &ast.types {
        for field in &declaration.fields {
            collect(&field.ty, &mut buffers);
        }
        for payload in declaration
            .variants
            .iter()
            .filter_map(|variant| variant.payload.as_ref())
        {
            collect(payload, &mut buffers);
        }
    }
    for function in &ast.functions {
        for parameter in &function.parameters {
            collect(&parameter.ty, &mut buffers);
        }
        collect(&function.result.ty, &mut buffers);
        collect_statement_types(&function.body, &mut |ty| collect(ty, &mut buffers));
    }
    buffers.into_values().collect()
}

fn collect_statement_types(statements: &[AstStatement], collect: &mut impl FnMut(&AstType)) {
    for statement in statements {
        match statement.kind() {
            AstStatement::Bind { ty: Some(ty), .. } => collect(ty),
            AstStatement::If {
                then_body,
                else_body,
                ..
            } => {
                collect_statement_types(then_body, collect);
                collect_statement_types(else_body, collect);
            }
            AstStatement::While { body, .. } => collect_statement_types(body, collect),
            AstStatement::Bind { ty: None, .. }
            | AstStatement::Return(_)
            | AstStatement::Expression(_)
            | AstStatement::Defer(_)
            | AstStatement::FieldSet { .. } => {}
            AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn resolve_and_typecheck_with_hosts(
    ast: AstModule,
    host_functions: BTreeMap<String, HostFunction>,
    host_interface_hash: Option<StableId>,
    schema_hash: Option<StableId>,
) -> Result<HirModule, CompileError> {
    if ast
        .functions
        .iter()
        .filter(|function| function.effect == FunctionEffect::Migration)
        .count()
        > 1
    {
        return Err(CompileError::InvalidReloadMetadata(
            "multiple migration entries",
        ));
    }
    let activation_count = ast
        .functions
        .iter()
        .filter(|function| function.is_activation)
        .count();
    if activation_count > 1 {
        return Err(CompileError::InvalidReloadMetadata(
            "multiple activation entries",
        ));
    }
    if ast
        .functions
        .iter()
        .any(|function| function.is_activation && function.effect != FunctionEffect::Immediate)
    {
        return Err(CompileError::InvalidReloadMetadata(
            "activation entry must have Immediate effect",
        ));
    }
    let mut enum_types = ast
        .types
        .iter()
        .filter(|declaration| declaration.kind == AstTypeKind::Enum)
        .map(|declaration| EnumType {
            type_id: StableId::from_name(&declaration.name),
            variants: declaration
                .variants
                .iter()
                .enumerate()
                .map(|(tag, variant)| EnumVariant {
                    stable_id: StableId::from_parts(&[&declaration.name, "::", &variant.name]),
                    tag: u32::try_from(tag).expect("enum variant count is parser bounded"),
                    payload_type: variant.payload.as_ref().map(lower_type),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    for async_result in host_functions
        .values()
        .filter_map(|function| function.metadata.async_result)
    {
        let enum_type = nexa_bytecode::result_type(async_result.success, async_result.error);
        if !enum_types
            .iter()
            .any(|candidate| candidate.type_id == enum_type.type_id)
        {
            enum_types.push(enum_type);
        }
    }
    collect_builtin_enum_types(&ast, &mut enum_types);
    let state_handle_targets = collect_state_handle_targets(&ast);
    let (array_types, map_types) = collect_collection_types(&ast);
    let buffer_types = collect_buffer_types(&ast);
    let declared_type_ids = ast
        .types
        .iter()
        .map(|declaration| StableId::from_name(&declaration.name))
        .collect::<BTreeSet<_>>();
    if map_types.iter().any(|map_type| {
        !matches!(
            map_type.key,
            ValueType::I32 | ValueType::I64 | ValueType::Rune | ValueType::String
        ) && !matches!(map_type.key, ValueType::Named(type_id) if
            state_handle_targets.contains_key(&type_id)
                || map_type.key == nexa_bytecode::stable_id_type()
                || !declared_type_ids.contains(&type_id))
    }) {
        return Err(CompileError::TypeMismatch);
    }
    for map_type in &map_types {
        let option = nexa_bytecode::option_type(map_type.value);
        if !enum_types
            .iter()
            .any(|candidate| candidate.type_id == option.type_id)
        {
            enum_types.push(option);
        }
    }
    if !state_handle_targets.is_empty() {
        enum_types.push(nexa_bytecode::state_handle_error_type());
        for target in state_handle_targets.values().copied() {
            let result = nexa_bytecode::result_type(
                target,
                ValueType::Named(nexa_bytecode::state_handle_error_type().type_id),
            );
            if !enum_types
                .iter()
                .any(|candidate| candidate.type_id == result.type_id)
            {
                enum_types.push(result);
            }
        }
    }
    let mut enum_variants = BTreeMap::new();
    for declaration in ast
        .types
        .iter()
        .filter(|declaration| declaration.kind == AstTypeKind::Enum)
    {
        let type_id = StableId::from_name(&declaration.name);
        for (source_variant, variant) in declaration.variants.iter().zip(
            enum_types
                .iter()
                .find(|enum_type| enum_type.type_id == type_id)
                .expect("source enum metadata was built")
                .variants
                .iter(),
        ) {
            enum_variants.insert((type_id, source_variant.name.clone()), variant.clone());
        }
    }
    for enum_type in &enum_types {
        for variant in &enum_type.variants {
            for name in [
                "None",
                "Some",
                "Ok",
                "Err",
                "WrongDomain",
                "Missing",
                "StaleGeneration",
                "GenerationExhausted",
            ] {
                if variant.stable_id == builtin_variant_id(name) {
                    enum_variants.insert((enum_type.type_id, name.to_owned()), variant.clone());
                }
            }
        }
    }
    let struct_types = ast
        .types
        .iter()
        .filter(|declaration| declaration.kind == AstTypeKind::Struct)
        .map(|declaration| StructType {
            type_id: StableId::from_name(&declaration.name),
            fields: declaration
                .fields
                .iter()
                .map(|field| BytecodeStructField {
                    stable_id: StableId::from_parts(&[&declaration.name, "::", &field.name]),
                    ty: lower_type(&field.ty),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let mut struct_fields = BTreeMap::new();
    for declaration in ast
        .types
        .iter()
        .filter(|declaration| declaration.kind == AstTypeKind::Struct)
    {
        let type_id = StableId::from_name(&declaration.name);
        let metadata = struct_types
            .iter()
            .find(|struct_type| struct_type.type_id == type_id)
            .expect("source struct metadata was built");
        for (source, field) in declaration.fields.iter().zip(&metadata.fields) {
            struct_fields.insert((type_id, source.name.clone()), *field);
        }
    }
    let class_types = ast
        .types
        .iter()
        .filter(|declaration| declaration.kind == AstTypeKind::Class)
        .map(|declaration| ClassType {
            type_id: StableId::from_name(&declaration.name),
            fields: declaration
                .fields
                .iter()
                .map(|field| BytecodeStructField {
                    stable_id: StableId::from_parts(&[&declaration.name, "::", &field.name]),
                    ty: lower_type(&field.ty),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let mut class_fields = BTreeMap::new();
    for declaration in ast
        .types
        .iter()
        .filter(|declaration| declaration.kind == AstTypeKind::Class)
    {
        let type_id = StableId::from_name(&declaration.name);
        let metadata = class_types
            .iter()
            .find(|class_type| class_type.type_id == type_id)
            .expect("source class metadata was built");
        for (source, field) in declaration.fields.iter().zip(&metadata.fields) {
            class_fields.insert((type_id, source.name.clone()), *field);
        }
    }
    let state_schema = StateSchema {
        types: ast
            .types
            .iter()
            .filter(|declaration| declaration.kind == AstTypeKind::StatefulClass)
            .map(|declaration| StateType {
                stable_id: StableId::from_name(&declaration.name),
                version: declaration.version,
                fields: declaration
                    .fields
                    .iter()
                    .map(|field| StateField {
                        stable_id: StableId::from_parts(&[&declaration.name, "::", &field.name]),
                        ty: lower_type(&field.ty),
                    })
                    .collect(),
            })
            .collect(),
    };
    let mut known_types = [
        "string",
        "rune",
        "Array",
        "Map",
        "Option",
        "Result",
        "Task",
        "Buffer",
        "HostRequest",
        "ResourceToken",
        "Snapshot",
        "StableId",
        "StateHandle",
        "StateHandleError",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    for declaration in &ast.types {
        if !known_types.insert(declaration.name.clone()) {
            return Err(CompileError::DuplicateName(declaration.name.clone()));
        }
    }
    for declaration in &ast.types {
        for field in &declaration.fields {
            validate_type(&field.ty, &known_types)?;
        }
        let mut variants = BTreeSet::new();
        let mut fields = BTreeSet::new();
        for field in &declaration.fields {
            if !fields.insert(&field.name) {
                return Err(CompileError::DuplicateName(format!(
                    "{}.{}",
                    declaration.name, field.name
                )));
            }
        }
        for variant in &declaration.variants {
            if !variants.insert(&variant.name) {
                return Err(CompileError::DuplicateName(format!(
                    "{}.{}",
                    declaration.name, variant.name
                )));
            }
            if let Some(payload) = &variant.payload {
                validate_type(payload, &known_types)?;
            }
        }
    }
    validate_state_handle_targets(&ast)?;
    validate_stateful_fields(&ast)?;
    let mut signatures = host_functions
        .iter()
        .map(|(name, function)| (name.clone(), function.signature.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut suspending_functions = host_functions
        .iter()
        .filter_map(|(name, function)| {
            (function.metadata.mode == HostCallMode::Async).then_some(name.clone())
        })
        .collect::<BTreeSet<_>>();
    for function in &ast.functions {
        if function.effect == FunctionEffect::Task {
            suspending_functions.insert(function.name.clone());
        }
        for parameter in &function.parameters {
            validate_type(&parameter.ty, &known_types)?;
        }
        validate_type(&function.result.ty, &known_types)?;
        validate_statement_types(&function.body, &known_types)?;
        let signature = Signature {
            parameters: function
                .parameters
                .iter()
                .map(|parameter| lower_type(&parameter.ty))
                .collect(),
            result: Some(lower_type(&function.result.ty)),
        };
        if signatures
            .insert(function.name.clone(), signature)
            .is_some()
        {
            return Err(CompileError::DuplicateName(function.name.clone()));
        }
    }
    let mut functions = Vec::new();
    let module_span = ast.span;
    for mut function in ast.functions {
        validate_awaits(&function.body, &suspending_functions)?;
        resolve_local_scopes(&mut function)?;
        let signature = signatures[&function.name].clone();
        let mut locals = BTreeMap::new();
        for (index, (parameter, ty)) in function
            .parameters
            .iter()
            .zip(signature.parameters.iter().copied())
            .enumerate()
        {
            let register = u16::try_from(index).map_err(|_| CompileError::TooManyRegisters)?;
            if locals
                .insert(parameter.name.clone(), (register, ty))
                .is_some()
            {
                return Err(CompileError::DuplicateName(parameter.name.clone()));
            }
        }
        if function.effect != FunctionEffect::Task && statements_contain_await(&function.body) {
            return Err(CompileError::InvalidEffect);
        }
        let mut next_register =
            u16::try_from(function.parameters.len()).map_err(|_| CompileError::TooManyRegisters)?;
        let type_context = TypeContext {
            signatures: &signatures,
            enum_types: &enum_types,
            enum_variants: &enum_variants,
            struct_types: &struct_types,
            struct_fields: &struct_fields,
            class_types: &class_types,
            class_fields: &class_fields,
            state_schema: &state_schema,
            function_result: signature.result.expect("result is required"),
            effect: function.effect,
            state_handle_targets: &state_handle_targets,
            array_types: &array_types,
            map_types: &map_types,
            buffer_types: &buffer_types,
        };
        let flow = check_statements(
            &function.body,
            &mut locals,
            &type_context,
            &mut next_register,
        )?;
        if flow == Flow::FallsThrough {
            return Err(CompileError::MissingReturn);
        }
        functions.push(HirFunction {
            name: function.name,
            is_activation: function.is_activation,
            effect: function.effect,
            signature,
            body: function.body,
            locals,
            span: function.span,
        });
    }
    Ok(HirModule {
        functions,
        host_functions,
        host_interface_hash,
        schema_hash,
        state_schema,
        enum_types,
        enum_variants,
        struct_types,
        struct_fields,
        class_types,
        class_fields,
        state_handle_targets,
        array_types,
        map_types,
        buffer_types,
        span: module_span,
    })
}

fn validate_awaits(
    statements: &[AstStatement],
    suspending_functions: &BTreeSet<String>,
) -> Result<(), CompileError> {
    for statement in statements {
        match statement.kind() {
            AstStatement::Bind { value, .. }
            | AstStatement::Return(value)
            | AstStatement::Expression(value)
            | AstStatement::Defer(value) => {
                validate_await_expression(value, suspending_functions, false)?;
            }
            AstStatement::FieldSet {
                value, replacement, ..
            } => {
                validate_await_expression(value, suspending_functions, false)?;
                validate_await_expression(replacement, suspending_functions, false)?;
            }
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                validate_await_expression(condition, suspending_functions, false)?;
                validate_awaits(then_body, suspending_functions)?;
                validate_awaits(else_body, suspending_functions)?;
            }
            AstStatement::While { condition, body } => {
                validate_await_expression(condition, suspending_functions, false)?;
                validate_awaits(body, suspending_functions)?;
            }
            AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
        }
    }
    Ok(())
}

fn validate_await_expression(
    expression: &AstExpression,
    suspending_functions: &BTreeSet<String>,
    awaited: bool,
) -> Result<(), CompileError> {
    match expression.kind() {
        AstExpression::Await(inner) => {
            let awaited = match inner.kind() {
                AstExpression::Try(expression) => expression.as_ref(),
                AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
                expression => expression,
            };
            let AstExpression::Call { function, .. } = awaited.kind() else {
                return Err(CompileError::InvalidEffect);
            };
            if !suspending_functions.contains(function) {
                return Err(CompileError::InvalidEffect);
            }
            validate_await_expression(inner, suspending_functions, true)
        }
        AstExpression::Call {
            function,
            arguments,
        } => {
            if suspending_functions.contains(function) && !awaited {
                return Err(CompileError::InvalidEffect);
            }
            for argument in arguments {
                validate_await_expression(argument, suspending_functions, false)?;
            }
            Ok(())
        }
        AstExpression::Binary { lhs, rhs, .. } => {
            validate_await_expression(lhs, suspending_functions, false)?;
            validate_await_expression(rhs, suspending_functions, false)
        }
        AstExpression::Constructor { payload, .. } => {
            if let Some(payload) = payload {
                validate_await_expression(payload, suspending_functions, false)?;
            }
            Ok(())
        }
        AstExpression::StructLiteral { fields, .. } | AstExpression::ClassNew { fields, .. } => {
            for field in fields {
                validate_await_expression(&field.value, suspending_functions, false)?;
            }
            Ok(())
        }
        AstExpression::FieldGet { value, .. } => {
            validate_await_expression(value, suspending_functions, false)
        }
        AstExpression::StructWith { value, updates } => {
            validate_await_expression(value, suspending_functions, false)?;
            for field in updates {
                validate_await_expression(&field.value, suspending_functions, false)?;
            }
            Ok(())
        }
        AstExpression::Match { value, arms } => {
            validate_await_expression(value, suspending_functions, false)?;
            for arm in arms {
                validate_await_expression(&arm.value, suspending_functions, false)?;
            }
            Ok(())
        }
        AstExpression::Try(expression) => {
            validate_await_expression(expression, suspending_functions, awaited)
        }
        AstExpression::Migration(intrinsic) => {
            for expression in migration_expressions(intrinsic) {
                validate_await_expression(expression, suspending_functions, false)?;
            }
            Ok(())
        }
        AstExpression::Integer(_)
        | AstExpression::Float(_)
        | AstExpression::Rune(_)
        | AstExpression::String(_)
        | AstExpression::Bool(_)
        | AstExpression::ArrayNew { .. }
        | AstExpression::MapNew { .. }
        | AstExpression::Name(_) => Ok(()),
        AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
    }
}

fn resolve_local_scopes(function: &mut AstFunction) -> Result<(), CompileError> {
    let mut root = BTreeMap::new();
    for parameter in &function.parameters {
        if root
            .insert(parameter.name.clone(), parameter.name.clone())
            .is_some()
        {
            return Err(CompileError::DuplicateName(parameter.name.clone()));
        }
    }
    let mut scopes = vec![root];
    let mut next_local = 0_u32;
    resolve_statements(&mut function.body, &mut scopes, &mut next_local)
}

fn resolve_statements(
    statements: &mut [AstStatement],
    scopes: &mut Vec<BTreeMap<String, String>>,
    next_local: &mut u32,
) -> Result<(), CompileError> {
    for statement in statements {
        match statement.kind_mut() {
            AstStatement::Bind { name, value, .. } => {
                resolve_expression(value, scopes, next_local)?;
                let source_name = name.clone();
                if scopes
                    .last()
                    .expect("a function always has a lexical scope")
                    .contains_key(&source_name)
                {
                    return Err(CompileError::DuplicateName(source_name));
                }
                let resolved = format!("{source_name}#{}", *next_local);
                *next_local = next_local
                    .checked_add(1)
                    .ok_or(CompileError::TooManyRegisters)?;
                scopes
                    .last_mut()
                    .expect("a function always has a lexical scope")
                    .insert(source_name, resolved.clone());
                *name = resolved;
            }
            AstStatement::Return(expression)
            | AstStatement::Expression(expression)
            | AstStatement::Defer(expression) => {
                resolve_expression(expression, scopes, next_local)?;
            }
            AstStatement::FieldSet {
                value, replacement, ..
            } => {
                resolve_expression(value, scopes, next_local)?;
                resolve_expression(replacement, scopes, next_local)?;
            }
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                resolve_expression(condition, scopes, next_local)?;
                scopes.push(BTreeMap::new());
                resolve_statements(then_body, scopes, next_local)?;
                scopes.pop();
                scopes.push(BTreeMap::new());
                resolve_statements(else_body, scopes, next_local)?;
                scopes.pop();
            }
            AstStatement::While { condition, body } => {
                resolve_expression(condition, scopes, next_local)?;
                scopes.push(BTreeMap::new());
                resolve_statements(body, scopes, next_local)?;
                scopes.pop();
            }
            AstStatement::Spanned { .. } => unreachable!("kind_mut strips spans"),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn resolve_expression(
    expression: &mut AstExpression,
    scopes: &mut Vec<BTreeMap<String, String>>,
    next_local: &mut u32,
) -> Result<(), CompileError> {
    match expression.kind_mut() {
        AstExpression::Name(name) => {
            let resolved = scopes
                .iter()
                .rev()
                .find_map(|scope| scope.get(name))
                .cloned();
            if let Some(resolved) = resolved {
                *name = resolved;
            } else if !name.chars().next().is_some_and(char::is_uppercase) {
                return Err(CompileError::UnknownName(name.clone()));
            }
        }
        AstExpression::Binary { lhs, rhs, .. } => {
            resolve_expression(lhs, scopes, next_local)?;
            resolve_expression(rhs, scopes, next_local)?;
        }
        AstExpression::Call {
            function,
            arguments,
        } => {
            let state_method = state_handle_method(function);
            if let Some((receiver, method)) = state_method {
                let resolved = scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(receiver))
                    .cloned()
                    .ok_or_else(|| CompileError::UnknownName(receiver.to_owned()))?;
                *function = format!(
                    "{resolved}.{}",
                    match method {
                        StateHandleMethod::Resolve => "resolve",
                        StateHandleMethod::IsAlive => "is_alive",
                        StateHandleMethod::StableId => "stable_id",
                        StateHandleMethod::Generation => "generation",
                        StateHandleMethod::Equality => "equality",
                        StateHandleMethod::Hash => "hash",
                    }
                );
            } else if let Some((receiver, method)) = buffer_method(function) {
                let resolved = scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(receiver))
                    .cloned()
                    .ok_or_else(|| CompileError::UnknownName(receiver.to_owned()))?;
                *function = format!(
                    "{resolved}.{}",
                    match method {
                        BufferMethod::Len => "len",
                        BufferMethod::Get => "get",
                        BufferMethod::Set => "set",
                        BufferMethod::Slice => "slice",
                        BufferMethod::Copy => "copy",
                    }
                );
            } else if let Some((receiver, method)) = map_method(function) {
                let resolved = scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(receiver))
                    .cloned()
                    .ok_or_else(|| CompileError::UnknownName(receiver.to_owned()))?;
                *function = format!(
                    "{resolved}.{}",
                    match method {
                        MapMethod::Len => "len",
                        MapMethod::Get => "get",
                        MapMethod::Set => "set",
                        MapMethod::Remove => "remove",
                        MapMethod::Contains => "contains",
                        MapMethod::Clear => "clear",
                    }
                );
            } else if let Some((receiver, method)) = array_method(function) {
                let resolved = scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(receiver))
                    .cloned()
                    .ok_or_else(|| CompileError::UnknownName(receiver.to_owned()))?;
                *function = format!(
                    "{resolved}.{}",
                    match method {
                        ArrayMethod::Len => "len",
                        ArrayMethod::Get => "get",
                        ArrayMethod::Set => "set",
                        ArrayMethod::Push => "push",
                        ArrayMethod::Pop => "pop",
                        ArrayMethod::Insert => "insert",
                        ArrayMethod::Remove => "remove",
                        ArrayMethod::Clear => "clear",
                    }
                );
            } else if let Some((receiver, method)) = string_method(function) {
                let resolved = scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(receiver))
                    .cloned()
                    .ok_or_else(|| CompileError::UnknownName(receiver.to_owned()))?;
                *function = format!(
                    "{resolved}.{}",
                    match method {
                        StringMethod::Len => "len",
                        StringMethod::ByteLen => "byte_len",
                        StringMethod::Equal => "equals",
                        StringMethod::Concat => "concat",
                        StringMethod::RuneAt => "rune_at",
                        StringMethod::Hash => "hash",
                    }
                );
            }
            for argument in arguments {
                resolve_expression(argument, scopes, next_local)?;
            }
        }
        AstExpression::Await(expression) | AstExpression::Try(expression) => {
            resolve_expression(expression, scopes, next_local)?;
        }
        AstExpression::Constructor { payload, .. } => {
            if let Some(payload) = payload {
                resolve_expression(payload, scopes, next_local)?;
            }
        }
        AstExpression::StructLiteral { fields, .. } | AstExpression::ClassNew { fields, .. } => {
            for field in fields {
                resolve_expression(&mut field.value, scopes, next_local)?;
            }
        }
        AstExpression::FieldGet { value, .. } => {
            resolve_expression(value, scopes, next_local)?;
        }
        AstExpression::StructWith { value, updates } => {
            resolve_expression(value, scopes, next_local)?;
            for field in updates {
                resolve_expression(&mut field.value, scopes, next_local)?;
            }
        }
        AstExpression::Match { value, arms } => {
            resolve_expression(value, scopes, next_local)?;
            for arm in arms {
                scopes.push(BTreeMap::new());
                if let Some(binding) = &mut arm.binding {
                    let source_name = binding.clone();
                    let resolved = format!("{source_name}#{}", *next_local);
                    *next_local = next_local
                        .checked_add(1)
                        .ok_or(CompileError::TooManyRegisters)?;
                    scopes
                        .last_mut()
                        .expect("match arm scope exists")
                        .insert(source_name, resolved.clone());
                    *binding = resolved;
                }
                resolve_expression(&mut arm.value, scopes, next_local)?;
                scopes.pop();
            }
        }
        AstExpression::Migration(intrinsic) => {
            for expression in migration_expressions_mut(intrinsic) {
                resolve_expression(expression, scopes, next_local)?;
            }
        }
        AstExpression::Integer(_)
        | AstExpression::Float(_)
        | AstExpression::Rune(_)
        | AstExpression::String(_)
        | AstExpression::Bool(_)
        | AstExpression::ArrayNew { .. }
        | AstExpression::MapNew { .. } => {}
        AstExpression::Spanned { .. } => unreachable!("kind_mut strips spans"),
    }
    Ok(())
}

fn validate_type(ty: &AstType, known_types: &BTreeSet<String>) -> Result<(), CompileError> {
    match ty.kind() {
        AstType::Named(name) if !known_types.contains(name) => {
            Err(CompileError::UnknownType(name.clone()))
        }
        AstType::BuiltinGeneric { name, arguments } => {
            let expected = match name.as_str() {
                "Option" | "StateHandle" | "Array" | "Buffer" => 1,
                "Result" | "Map" => 2,
                _ => return Err(CompileError::UnknownType(name.clone())),
            };
            if arguments.len() != expected {
                return Err(CompileError::TypeMismatch);
            }
            for argument in arguments {
                validate_type(argument, known_types)?;
            }
            Ok(())
        }
        AstType::I32
        | AstType::I64
        | AstType::F32
        | AstType::F64
        | AstType::Bool
        | AstType::Rune
        | AstType::String
        | AstType::Named(_) => Ok(()),
        AstType::Spanned { .. } => unreachable!("kind strips spans"),
    }
}

fn validate_stateful_fields(ast: &AstModule) -> Result<(), CompileError> {
    let declarations = ast
        .types
        .iter()
        .map(|declaration| (declaration.name.as_str(), declaration))
        .collect::<BTreeMap<_, _>>();
    for declaration in ast
        .types
        .iter()
        .filter(|declaration| declaration.kind == AstTypeKind::StatefulClass)
    {
        for field in &declaration.fields {
            validate_state_storage_type(&field.ty, &declarations, &mut BTreeSet::new())?;
        }
    }
    Ok(())
}

fn validate_state_handle_targets(ast: &AstModule) -> Result<(), CompileError> {
    let stateful_types = ast
        .types
        .iter()
        .filter(|declaration| declaration.kind == AstTypeKind::StatefulClass)
        .map(|declaration| declaration.name.as_str())
        .collect::<BTreeSet<_>>();
    let validate = |ty: &AstType| validate_state_handle_target(ty, &stateful_types);
    for declaration in &ast.types {
        for field in &declaration.fields {
            validate(&field.ty)?;
        }
        for variant in &declaration.variants {
            if let Some(payload) = &variant.payload {
                validate(payload)?;
            }
        }
    }
    for function in &ast.functions {
        for parameter in &function.parameters {
            validate(&parameter.ty)?;
        }
        validate(&function.result.ty)?;
        let mut error = None;
        collect_statement_types(&function.body, &mut |ty| {
            if error.is_none() {
                error = validate(ty).err();
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
    }
    Ok(())
}

fn validate_state_handle_target(
    ty: &AstType,
    stateful_types: &BTreeSet<&str>,
) -> Result<(), CompileError> {
    let AstType::BuiltinGeneric { name, arguments } = ty.kind() else {
        return Ok(());
    };
    for argument in arguments {
        validate_state_handle_target(argument, stateful_types)?;
    }
    if name == "StateHandle" {
        let [target] = arguments.as_slice() else {
            return Err(CompileError::TypeMismatch);
        };
        let AstType::Named(target) = target.kind() else {
            return Err(CompileError::TypeMismatch);
        };
        if !stateful_types.contains(target.as_str()) {
            return Err(CompileError::TypeMismatch);
        }
    }
    Ok(())
}

fn validate_state_storage_type<'a>(
    ty: &AstType,
    declarations: &BTreeMap<&'a str, &'a AstTypeDeclaration>,
    visiting: &mut BTreeSet<&'a str>,
) -> Result<(), CompileError> {
    match ty.kind() {
        AstType::I32
        | AstType::I64
        | AstType::F32
        | AstType::F64
        | AstType::Bool
        | AstType::Rune
        | AstType::String => Ok(()),
        AstType::Named(name) => {
            let declaration = declarations
                .get(name.as_str())
                .ok_or(CompileError::TypeMismatch)?;
            if !matches!(declaration.kind, AstTypeKind::Struct | AstTypeKind::Enum) {
                return Err(CompileError::TypeMismatch);
            }
            if !visiting.insert(declaration.name.as_str()) {
                return Ok(());
            }
            for field in &declaration.fields {
                validate_state_storage_type(&field.ty, declarations, visiting)?;
            }
            for variant in &declaration.variants {
                if let Some(payload) = &variant.payload {
                    validate_state_storage_type(payload, declarations, visiting)?;
                }
            }
            visiting.remove(declaration.name.as_str());
            Ok(())
        }
        AstType::BuiltinGeneric { name, arguments }
            if matches!(name.as_str(), "Option" | "Result") =>
        {
            for argument in arguments {
                validate_state_storage_type(argument, declarations, visiting)?;
            }
            Ok(())
        }
        AstType::BuiltinGeneric { name, arguments }
            if name == "StateHandle" && arguments.len() == 1 =>
        {
            let AstType::Named(target) = arguments[0].kind() else {
                return Err(CompileError::TypeMismatch);
            };
            declarations
                .get(target.as_str())
                .filter(|declaration| declaration.kind == AstTypeKind::StatefulClass)
                .map(|_| ())
                .ok_or(CompileError::TypeMismatch)
        }
        AstType::BuiltinGeneric { .. } => Err(CompileError::TypeMismatch),
        AstType::Spanned { .. } => unreachable!("kind strips spans"),
    }
}

fn validate_statement_types(
    statements: &[AstStatement],
    known_types: &BTreeSet<String>,
) -> Result<(), CompileError> {
    for statement in statements {
        match statement.kind() {
            AstStatement::Bind { ty: Some(ty), .. } => validate_type(ty, known_types)?,
            AstStatement::If {
                then_body,
                else_body,
                ..
            } => {
                validate_statement_types(then_body, known_types)?;
                validate_statement_types(else_body, known_types)?;
            }
            AstStatement::While { body, .. } => {
                validate_statement_types(body, known_types)?;
            }
            AstStatement::Bind { ty: None, .. }
            | AstStatement::Return(_)
            | AstStatement::Expression(_)
            | AstStatement::Defer(_)
            | AstStatement::FieldSet { .. } => {}
            AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    FallsThrough,
    Returns,
    Diverges,
}

struct TypeContext<'a> {
    signatures: &'a BTreeMap<String, Signature>,
    enum_types: &'a [EnumType],
    enum_variants: &'a BTreeMap<(StableId, String), EnumVariant>,
    struct_types: &'a [StructType],
    struct_fields: &'a BTreeMap<(StableId, String), BytecodeStructField>,
    class_types: &'a [ClassType],
    class_fields: &'a BTreeMap<(StableId, String), BytecodeStructField>,
    state_schema: &'a StateSchema,
    function_result: ValueType,
    effect: FunctionEffect,
    state_handle_targets: &'a BTreeMap<StableId, ValueType>,
    array_types: &'a [ArrayType],
    map_types: &'a [MapType],
    buffer_types: &'a [BufferType],
}

fn check_statements(
    statements: &[AstStatement],
    locals: &mut BTreeMap<String, (u16, ValueType)>,
    context: &TypeContext<'_>,
    next_register: &mut u16,
) -> Result<Flow, CompileError> {
    let mut flow = Flow::FallsThrough;
    for statement in statements {
        match statement.kind() {
            AstStatement::Bind { name, ty, value } => {
                let expected = ty.as_ref().map(lower_type);
                let actual = expression_type(value, locals, context, next_register, expected)?;
                if expected.is_some_and(|expected| expected != actual) {
                    return Err(CompileError::TypeMismatch);
                }
                let register = *next_register;
                *next_register = next_register
                    .checked_add(1)
                    .ok_or(CompileError::TooManyRegisters)?;
                if locals.insert(name.clone(), (register, actual)).is_some() {
                    return Err(CompileError::DuplicateName(name.clone()));
                }
            }
            AstStatement::Return(expression) => {
                if expression_type(
                    expression,
                    locals,
                    context,
                    next_register,
                    Some(context.function_result),
                )? != context.function_result
                {
                    return Err(CompileError::TypeMismatch);
                }
                flow = Flow::Returns;
            }
            AstStatement::Expression(expression) => {
                expression_type(expression, locals, context, next_register, None)?;
            }
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                if expression_type(
                    condition,
                    locals,
                    context,
                    next_register,
                    Some(ValueType::Bool),
                )? != ValueType::Bool
                {
                    return Err(CompileError::TypeMismatch);
                }
                let mut then_locals = locals.clone();
                let mut else_locals = locals.clone();
                let then_flow =
                    check_statements(then_body, &mut then_locals, context, next_register)?;
                let else_flow =
                    check_statements(else_body, &mut else_locals, context, next_register)?;
                for (name, binding) in then_locals.into_iter().chain(else_locals) {
                    locals.entry(name).or_insert(binding);
                }
                if then_flow == Flow::Returns && else_flow == Flow::Returns {
                    flow = Flow::Returns;
                } else if then_flow != Flow::FallsThrough && else_flow != Flow::FallsThrough {
                    flow = Flow::Diverges;
                }
            }
            AstStatement::While { condition, body } => {
                if expression_type(
                    condition,
                    locals,
                    context,
                    next_register,
                    Some(ValueType::Bool),
                )? != ValueType::Bool
                {
                    return Err(CompileError::TypeMismatch);
                }
                let mut loop_locals = locals.clone();
                let body_flow = check_statements(body, &mut loop_locals, context, next_register)?;
                for (name, binding) in loop_locals {
                    locals.entry(name).or_insert(binding);
                }
                if matches!(condition.kind(), AstExpression::Bool(true))
                    && body_flow != Flow::FallsThrough
                {
                    flow = body_flow;
                }
            }
            AstStatement::Defer(expression) => {
                if contains_await(expression) {
                    return Err(CompileError::SuspendingDefer);
                }
                expression_type(expression, locals, context, next_register, None)?;
            }
            AstStatement::FieldSet {
                value,
                field,
                replacement,
            } => check_field_set(value, field, replacement, locals, context, next_register)?,
            AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
        }
    }
    Ok(flow)
}

fn check_field_set(
    value: &AstExpression,
    field: &str,
    replacement: &AstExpression,
    locals: &mut BTreeMap<String, (u16, ValueType)>,
    context: &TypeContext<'_>,
    next_register: &mut u16,
) -> Result<(), CompileError> {
    let ValueType::Named(type_id) = expression_type(value, locals, context, next_register, None)?
    else {
        return Err(CompileError::TypeMismatch);
    };
    let field = context
        .class_fields
        .get(&(type_id, field.to_owned()))
        .ok_or(CompileError::TypeMismatch)?;
    if expression_type(replacement, locals, context, next_register, Some(field.ty))? != field.ty {
        return Err(CompileError::TypeMismatch);
    }
    Ok(())
}

fn statements_contain_await(statements: &[AstStatement]) -> bool {
    statements.iter().any(|statement| match statement.kind() {
        AstStatement::Bind { value, .. }
        | AstStatement::Return(value)
        | AstStatement::Expression(value)
        | AstStatement::Defer(value) => contains_await(value),
        AstStatement::FieldSet {
            value, replacement, ..
        } => contains_await(value) || contains_await(replacement),
        AstStatement::If {
            condition,
            then_body,
            else_body,
        } => {
            contains_await(condition)
                || statements_contain_await(then_body)
                || statements_contain_await(else_body)
        }
        AstStatement::While { condition, body } => {
            contains_await(condition) || statements_contain_await(body)
        }
        AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
    })
}

fn contains_await(expression: &AstExpression) -> bool {
    match expression.kind() {
        AstExpression::Await(_) => true,
        AstExpression::Binary { lhs, rhs, .. } => contains_await(lhs) || contains_await(rhs),
        AstExpression::Call { arguments, .. } => arguments.iter().any(contains_await),
        AstExpression::Constructor { payload, .. } => {
            payload.as_deref().is_some_and(contains_await)
        }
        AstExpression::StructLiteral { fields, .. } | AstExpression::ClassNew { fields, .. } => {
            fields.iter().any(|field| contains_await(&field.value))
        }
        AstExpression::FieldGet { value, .. } => contains_await(value),
        AstExpression::StructWith { value, updates } => {
            contains_await(value) || updates.iter().any(|field| contains_await(&field.value))
        }
        AstExpression::Match { value, arms } => {
            contains_await(value) || arms.iter().any(|arm| contains_await(&arm.value))
        }
        AstExpression::Try(expression) => contains_await(expression),
        AstExpression::Migration(intrinsic) => migration_expressions(intrinsic)
            .into_iter()
            .any(contains_await),
        AstExpression::Integer(_)
        | AstExpression::Float(_)
        | AstExpression::Rune(_)
        | AstExpression::String(_)
        | AstExpression::Bool(_)
        | AstExpression::ArrayNew { .. }
        | AstExpression::MapNew { .. }
        | AstExpression::Name(_) => false,
        AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
    }
}

#[allow(clippy::too_many_lines)]
fn expression_type(
    expression: &AstExpression,
    locals: &mut BTreeMap<String, (u16, ValueType)>,
    context: &TypeContext<'_>,
    next_register: &mut u16,
    expected: Option<ValueType>,
) -> Result<ValueType, CompileError> {
    let actual = match expression.kind() {
        AstExpression::Integer(value) => match expected {
            Some(ValueType::I32) => {
                i32::try_from(*value).map_err(|_| CompileError::InvalidNumericConversion {
                    span: expression.span(),
                })?;
                ValueType::I32
            }
            Some(ValueType::I64) => ValueType::I64,
            Some(_) => return Err(CompileError::TypeMismatch),
            None if i32::try_from(*value).is_ok() => ValueType::I32,
            None => ValueType::I64,
        },
        AstExpression::Float(_) => match expected {
            Some(ValueType::F32) => ValueType::F32,
            Some(ValueType::F64) | None => ValueType::F64,
            Some(_) => return Err(CompileError::TypeMismatch),
        },
        AstExpression::Rune(value) => {
            if char::from_u32(*value).is_none() {
                return Err(CompileError::TypeMismatch);
            }
            ValueType::Rune
        }
        AstExpression::String(_) => ValueType::String,
        AstExpression::Bool(_) => ValueType::Bool,
        AstExpression::Name(name) => {
            if let Some((_, ty)) = locals.get(name) {
                *ty
            } else if let Some(ValueType::Named(type_id)) = expected
                && context
                    .enum_variants
                    .get(&(type_id, name.clone()))
                    .is_some_and(|variant| variant.payload_type.is_none())
            {
                ValueType::Named(type_id)
            } else {
                return Err(CompileError::UnknownName(name.clone()));
            }
        }
        AstExpression::StructLiteral { type_name, fields } => {
            let type_id = StableId::from_name(type_name);
            let struct_type = context
                .struct_types
                .iter()
                .find(|struct_type| struct_type.type_id == type_id)
                .ok_or_else(|| CompileError::UnknownType(type_name.clone()))?;
            if fields.len() != struct_type.fields.len() {
                return Err(CompileError::TypeMismatch);
            }
            let mut supplied = BTreeSet::new();
            for field in fields {
                let metadata = context
                    .struct_fields
                    .get(&(type_id, field.name.clone()))
                    .ok_or(CompileError::TypeMismatch)?;
                if !supplied.insert(metadata.stable_id)
                    || expression_type(
                        &field.value,
                        locals,
                        context,
                        next_register,
                        Some(metadata.ty),
                    )? != metadata.ty
                {
                    return Err(CompileError::TypeMismatch);
                }
            }
            ValueType::Named(type_id)
        }
        AstExpression::ClassNew { type_name, fields } => {
            let type_id = StableId::from_name(type_name);
            let class_type = context
                .class_types
                .iter()
                .find(|class_type| class_type.type_id == type_id)
                .ok_or_else(|| CompileError::UnknownType(type_name.clone()))?;
            if fields.len() != class_type.fields.len() {
                return Err(CompileError::TypeMismatch);
            }
            let mut supplied = BTreeSet::new();
            for field in fields {
                let metadata = context
                    .class_fields
                    .get(&(type_id, field.name.clone()))
                    .ok_or(CompileError::TypeMismatch)?;
                if !supplied.insert(metadata.stable_id)
                    || expression_type(
                        &field.value,
                        locals,
                        context,
                        next_register,
                        Some(metadata.ty),
                    )? != metadata.ty
                {
                    return Err(CompileError::TypeMismatch);
                }
            }
            ValueType::Named(type_id)
        }
        AstExpression::ArrayNew { element_type } => {
            if matches!(
                context.effect,
                FunctionEffect::Immediate | FunctionEffect::Migration | FunctionEffect::Cleanup
            ) {
                return Err(CompileError::InvalidEffect);
            }
            let element = lower_type(element_type);
            let type_id = nexa_bytecode::array_type(element);
            if !context
                .array_types
                .iter()
                .any(|array_type| array_type.type_id == type_id && array_type.element == element)
            {
                return Err(CompileError::TypeMismatch);
            }
            ValueType::Named(type_id)
        }
        AstExpression::MapNew {
            key_type,
            value_type,
        } => {
            if matches!(
                context.effect,
                FunctionEffect::Immediate | FunctionEffect::Migration | FunctionEffect::Cleanup
            ) {
                return Err(CompileError::InvalidEffect);
            }
            let key = lower_type(key_type);
            let value = lower_type(value_type);
            let type_id = nexa_bytecode::map_type(key, value);
            if !context.map_types.iter().any(|map_type| {
                map_type.type_id == type_id && map_type.key == key && map_type.value == value
            }) {
                return Err(CompileError::TypeMismatch);
            }
            ValueType::Named(type_id)
        }
        AstExpression::FieldGet { value, field } => {
            let ValueType::Named(type_id) =
                expression_type(value, locals, context, next_register, None)?
            else {
                return Err(CompileError::TypeMismatch);
            };
            context
                .struct_fields
                .get(&(type_id, field.clone()))
                .or_else(|| context.class_fields.get(&(type_id, field.clone())))
                .map(|field| field.ty)
                .ok_or(CompileError::TypeMismatch)?
        }
        AstExpression::StructWith { value, updates } => {
            let ValueType::Named(type_id) =
                expression_type(value, locals, context, next_register, None)?
            else {
                return Err(CompileError::TypeMismatch);
            };
            if !context
                .struct_types
                .iter()
                .any(|struct_type| struct_type.type_id == type_id)
                || updates.is_empty()
            {
                return Err(CompileError::TypeMismatch);
            }
            let mut supplied = BTreeSet::new();
            for update in updates {
                let field = context
                    .struct_fields
                    .get(&(type_id, update.name.clone()))
                    .ok_or(CompileError::TypeMismatch)?;
                if !supplied.insert(field.stable_id)
                    || expression_type(
                        &update.value,
                        locals,
                        context,
                        next_register,
                        Some(field.ty),
                    )? != field.ty
                {
                    return Err(CompileError::TypeMismatch);
                }
            }
            ValueType::Named(type_id)
        }
        AstExpression::Binary { op, lhs, rhs } if op.kind == BinaryOp::Equal => {
            let operand_type = expression_type(lhs, locals, context, next_register, None)?;
            let supported = operand_type == ValueType::String
                || matches!(operand_type, ValueType::Named(type_id) if context
                    .struct_types
                    .iter()
                    .any(|struct_type| struct_type.type_id == type_id)
                    || context
                        .class_types
                        .iter()
                        .any(|class_type| class_type.type_id == type_id));
            if !supported
                || expression_type(rhs, locals, context, next_register, Some(operand_type))?
                    != operand_type
            {
                return Err(CompileError::TypeMismatch);
            }
            ValueType::Bool
        }
        AstExpression::Binary { op, lhs, rhs } => {
            let numeric_type = if let Some(expected) = expected {
                expected
            } else {
                expression_type(lhs, locals, context, next_register, None)?
            };
            if op.kind == BinaryOp::Add && numeric_type == ValueType::String {
                if expected.is_some()
                    && expression_type(
                        lhs,
                        locals,
                        context,
                        next_register,
                        Some(ValueType::String),
                    )? != ValueType::String
                {
                    return Err(CompileError::TypeMismatch);
                }
                if expression_type(rhs, locals, context, next_register, Some(ValueType::String))?
                    != ValueType::String
                {
                    return Err(CompileError::TypeMismatch);
                }
                ValueType::String
            } else {
                if !matches!(
                    numeric_type,
                    ValueType::I32 | ValueType::I64 | ValueType::F32 | ValueType::F64
                ) {
                    return Err(CompileError::TypeMismatch);
                }
                if expected.is_some()
                    && expression_type(lhs, locals, context, next_register, Some(numeric_type))?
                        != numeric_type
                {
                    return Err(CompileError::TypeMismatch);
                }
                if expression_type(rhs, locals, context, next_register, Some(numeric_type))?
                    != numeric_type
                {
                    return Err(CompileError::TypeMismatch);
                }
                numeric_type
            }
        }
        AstExpression::Call {
            function,
            arguments,
        } => {
            if let Some(ValueType::Named(type_id)) = expected
                && let Some(variant) = context.enum_variants.get(&(type_id, function.clone()))
            {
                let [payload] = arguments.as_slice() else {
                    return Err(CompileError::TypeMismatch);
                };
                let payload_type = variant.payload_type.ok_or(CompileError::TypeMismatch)?;
                if expression_type(payload, locals, context, next_register, Some(payload_type))?
                    != payload_type
                {
                    return Err(CompileError::TypeMismatch);
                }
                return Ok(ValueType::Named(type_id));
            }
            if let Some((receiver, method)) = string_method(function)
                && locals.get(receiver).map(|(_, ty)| *ty) == Some(ValueType::String)
            {
                let result = match method {
                    StringMethod::Len | StringMethod::ByteLen | StringMethod::Hash => {
                        if !arguments.is_empty() {
                            return Err(CompileError::TypeMismatch);
                        }
                        if method == StringMethod::Hash {
                            ValueType::I64
                        } else {
                            ValueType::I32
                        }
                    }
                    StringMethod::Equal | StringMethod::Concat => {
                        if arguments.len() != 1
                            || expression_type(
                                &arguments[0],
                                locals,
                                context,
                                next_register,
                                Some(ValueType::String),
                            )? != ValueType::String
                        {
                            return Err(CompileError::TypeMismatch);
                        }
                        if method == StringMethod::Equal {
                            ValueType::Bool
                        } else {
                            ValueType::String
                        }
                    }
                    StringMethod::RuneAt => {
                        if arguments.len() != 1
                            || expression_type(
                                &arguments[0],
                                locals,
                                context,
                                next_register,
                                Some(ValueType::I32),
                            )? != ValueType::I32
                        {
                            return Err(CompileError::TypeMismatch);
                        }
                        ValueType::Rune
                    }
                };
                if expected.is_some_and(|expected| expected != result) {
                    return Err(CompileError::TypeMismatch);
                }
                return Ok(result);
            }
            if let Some((receiver, method)) = buffer_method(function)
                && locals.get(receiver).is_some_and(|(_, ty)| {
                    matches!(ty, ValueType::Named(type_id) if context
                        .buffer_types
                        .iter()
                        .any(|buffer| buffer.type_id == *type_id))
                })
            {
                let receiver_type = locals
                    .get(receiver)
                    .map(|(_, ty)| *ty)
                    .ok_or_else(|| CompileError::UnknownName(receiver.to_owned()))?;
                let ValueType::Named(buffer_type) = receiver_type else {
                    return Err(CompileError::TypeMismatch);
                };
                let element = context
                    .buffer_types
                    .iter()
                    .find(|buffer| buffer.type_id == buffer_type)
                    .map(|buffer| buffer.element)
                    .ok_or(CompileError::TypeMismatch)?;
                if matches!(
                    context.effect,
                    FunctionEffect::Immediate | FunctionEffect::Migration | FunctionEffect::Cleanup
                ) {
                    return Err(CompileError::InvalidEffect);
                }
                let index = |argument: &AstExpression,
                             locals: &mut BTreeMap<String, (u16, ValueType)>,
                             next_register: &mut u16| {
                    expression_type(
                        argument,
                        locals,
                        context,
                        next_register,
                        Some(ValueType::I32),
                    )
                };
                let actual = match method {
                    BufferMethod::Len if arguments.is_empty() => ValueType::I32,
                    BufferMethod::Get if arguments.len() == 1 => {
                        if index(&arguments[0], locals, next_register)? != ValueType::I32 {
                            return Err(CompileError::TypeMismatch);
                        }
                        element
                    }
                    BufferMethod::Set if arguments.len() == 2 => {
                        if index(&arguments[0], locals, next_register)? != ValueType::I32
                            || expression_type(
                                &arguments[1],
                                locals,
                                context,
                                next_register,
                                Some(element),
                            )? != element
                        {
                            return Err(CompileError::TypeMismatch);
                        }
                        ValueType::Bool
                    }
                    BufferMethod::Slice if arguments.len() == 2 => {
                        if index(&arguments[0], locals, next_register)? != ValueType::I32
                            || index(&arguments[1], locals, next_register)? != ValueType::I32
                        {
                            return Err(CompileError::TypeMismatch);
                        }
                        receiver_type
                    }
                    BufferMethod::Copy if arguments.len() == 4 => {
                        if expression_type(
                            &arguments[0],
                            locals,
                            context,
                            next_register,
                            Some(receiver_type),
                        )? != receiver_type
                        {
                            return Err(CompileError::TypeMismatch);
                        }
                        for argument in &arguments[1..] {
                            if index(argument, locals, next_register)? != ValueType::I32 {
                                return Err(CompileError::TypeMismatch);
                            }
                        }
                        ValueType::Bool
                    }
                    _ => return Err(CompileError::TypeMismatch),
                };
                if expected.is_some_and(|expected| expected != actual) {
                    return Err(CompileError::TypeMismatch);
                }
                return Ok(actual);
            }
            if let Some((receiver, method)) = map_method(function)
                && locals.get(receiver).is_some_and(|(_, ty)| {
                    matches!(ty, ValueType::Named(type_id) if context
                        .map_types
                        .iter()
                        .any(|map_type| map_type.type_id == *type_id))
                })
            {
                let receiver_type = locals
                    .get(receiver)
                    .map(|(_, ty)| *ty)
                    .ok_or_else(|| CompileError::UnknownName(receiver.to_owned()))?;
                let ValueType::Named(map_type) = receiver_type else {
                    return Err(CompileError::TypeMismatch);
                };
                let map_type = context
                    .map_types
                    .iter()
                    .find(|candidate| candidate.type_id == map_type)
                    .ok_or(CompileError::TypeMismatch)?;
                if matches!(
                    context.effect,
                    FunctionEffect::Immediate | FunctionEffect::Migration | FunctionEffect::Cleanup
                ) {
                    return Err(CompileError::InvalidEffect);
                }
                let mut check_key = |argument: &AstExpression| {
                    expression_type(argument, locals, context, next_register, Some(map_type.key))
                };
                let actual = match method {
                    MapMethod::Len if arguments.is_empty() => ValueType::I32,
                    MapMethod::Get | MapMethod::Remove if arguments.len() == 1 => {
                        if check_key(&arguments[0])? != map_type.key {
                            return Err(CompileError::TypeMismatch);
                        }
                        ValueType::Named(nexa_bytecode::option_type(map_type.value).type_id)
                    }
                    MapMethod::Set if arguments.len() == 2 => {
                        if check_key(&arguments[0])? != map_type.key
                            || expression_type(
                                &arguments[1],
                                locals,
                                context,
                                next_register,
                                Some(map_type.value),
                            )? != map_type.value
                        {
                            return Err(CompileError::TypeMismatch);
                        }
                        ValueType::Bool
                    }
                    MapMethod::Contains if arguments.len() == 1 => {
                        if check_key(&arguments[0])? != map_type.key {
                            return Err(CompileError::TypeMismatch);
                        }
                        ValueType::Bool
                    }
                    MapMethod::Clear if arguments.is_empty() => ValueType::Bool,
                    _ => return Err(CompileError::TypeMismatch),
                };
                if expected.is_some_and(|expected| expected != actual) {
                    return Err(CompileError::TypeMismatch);
                }
                return Ok(actual);
            }
            if let Some((receiver, method)) = array_method(function) {
                let receiver_type = locals
                    .get(receiver)
                    .map(|(_, ty)| *ty)
                    .ok_or_else(|| CompileError::UnknownName(receiver.to_owned()))?;
                let ValueType::Named(array_type) = receiver_type else {
                    return Err(CompileError::TypeMismatch);
                };
                let element = context
                    .array_types
                    .iter()
                    .find(|candidate| candidate.type_id == array_type)
                    .map(|candidate| candidate.element)
                    .ok_or(CompileError::TypeMismatch)?;
                if matches!(
                    context.effect,
                    FunctionEffect::Immediate | FunctionEffect::Migration | FunctionEffect::Cleanup
                ) {
                    return Err(CompileError::InvalidEffect);
                }
                let actual = match method {
                    ArrayMethod::Len if arguments.is_empty() => ValueType::I32,
                    ArrayMethod::Get | ArrayMethod::Remove if arguments.len() == 1 => {
                        if expression_type(
                            &arguments[0],
                            locals,
                            context,
                            next_register,
                            Some(ValueType::I32),
                        )? != ValueType::I32
                        {
                            return Err(CompileError::TypeMismatch);
                        }
                        element
                    }
                    ArrayMethod::Set | ArrayMethod::Insert if arguments.len() == 2 => {
                        if expression_type(
                            &arguments[0],
                            locals,
                            context,
                            next_register,
                            Some(ValueType::I32),
                        )? != ValueType::I32
                            || expression_type(
                                &arguments[1],
                                locals,
                                context,
                                next_register,
                                Some(element),
                            )? != element
                        {
                            return Err(CompileError::TypeMismatch);
                        }
                        ValueType::Bool
                    }
                    ArrayMethod::Push if arguments.len() == 1 => {
                        if expression_type(
                            &arguments[0],
                            locals,
                            context,
                            next_register,
                            Some(element),
                        )? != element
                        {
                            return Err(CompileError::TypeMismatch);
                        }
                        ValueType::Bool
                    }
                    ArrayMethod::Pop if arguments.is_empty() => element,
                    ArrayMethod::Clear if arguments.is_empty() => ValueType::Bool,
                    _ => return Err(CompileError::TypeMismatch),
                };
                if expected.is_some_and(|expected| expected != actual) {
                    return Err(CompileError::TypeMismatch);
                }
                return Ok(actual);
            }
            if let Some((receiver, method)) = state_handle_method(function) {
                if matches!(
                    context.effect,
                    FunctionEffect::Migration | FunctionEffect::Cleanup
                ) {
                    return Err(CompileError::InvalidEffect);
                }
                let receiver_type = locals
                    .get(receiver)
                    .map(|(_, ty)| *ty)
                    .ok_or_else(|| CompileError::UnknownName(receiver.to_owned()))?;
                let ValueType::Named(handle_type) = receiver_type else {
                    return Err(CompileError::TypeMismatch);
                };
                let target = *context
                    .state_handle_targets
                    .get(&handle_type)
                    .ok_or(CompileError::TypeMismatch)?;
                let actual = match method {
                    StateHandleMethod::Equality => {
                        if arguments.len() != 1
                            || expression_type(
                                &arguments[0],
                                locals,
                                context,
                                next_register,
                                Some(receiver_type),
                            )? != receiver_type
                        {
                            Err(CompileError::TypeMismatch)
                        } else {
                            Ok(ValueType::Bool)
                        }
                    }
                    StateHandleMethod::Resolve => {
                        if arguments.is_empty() {
                            Ok(ValueType::Named(
                                nexa_bytecode::result_type(
                                    target,
                                    ValueType::Named(
                                        nexa_bytecode::state_handle_error_type().type_id,
                                    ),
                                )
                                .type_id,
                            ))
                        } else {
                            Err(CompileError::TypeMismatch)
                        }
                    }
                    StateHandleMethod::IsAlive => {
                        if arguments.is_empty() {
                            Ok(ValueType::Bool)
                        } else {
                            Err(CompileError::TypeMismatch)
                        }
                    }
                    StateHandleMethod::StableId => {
                        if arguments.is_empty() {
                            Ok(nexa_bytecode::stable_id_type())
                        } else {
                            Err(CompileError::TypeMismatch)
                        }
                    }
                    StateHandleMethod::Generation | StateHandleMethod::Hash => {
                        if arguments.is_empty() {
                            Ok(ValueType::I32)
                        } else {
                            Err(CompileError::TypeMismatch)
                        }
                    }
                }?;
                if expected.is_some_and(|expected| expected != actual) {
                    return Err(CompileError::TypeMismatch);
                }
                return Ok(actual);
            }
            let signature = context
                .signatures
                .get(function)
                .ok_or_else(|| CompileError::UnknownName(function.clone()))?;
            if arguments.len() != signature.parameters.len() {
                return Err(CompileError::TypeMismatch);
            }
            for (argument, parameter) in arguments.iter().zip(&signature.parameters) {
                if expression_type(argument, locals, context, next_register, Some(*parameter))?
                    != *parameter
                {
                    return Err(CompileError::TypeMismatch);
                }
            }
            signature.result.expect("functions have results")
        }
        AstExpression::Await(expression) => {
            expression_type(expression, locals, context, next_register, expected)?
        }
        AstExpression::Constructor {
            type_name,
            variant,
            payload,
        } => {
            let type_id = if let Some(type_name) = type_name {
                let type_id = StableId::from_name(type_name);
                if expected.is_some_and(|expected| expected != ValueType::Named(type_id)) {
                    return Err(CompileError::TypeMismatch);
                }
                type_id
            } else {
                let ValueType::Named(type_id) = expected.ok_or(CompileError::CannotInferType)?
                else {
                    return Err(CompileError::TypeMismatch);
                };
                type_id
            };
            let metadata = context
                .enum_variants
                .get(&(type_id, variant.clone()))
                .ok_or(CompileError::TypeMismatch)?;
            match (payload, metadata.payload_type) {
                (Some(payload), Some(payload_type)) => {
                    if expression_type(payload, locals, context, next_register, Some(payload_type))?
                        != payload_type
                    {
                        return Err(CompileError::TypeMismatch);
                    }
                }
                (None, None) => {}
                _ => return Err(CompileError::TypeMismatch),
            }
            ValueType::Named(type_id)
        }
        AstExpression::Match { value, arms } => {
            let ValueType::Named(type_id) =
                expression_type(value, locals, context, next_register, None)?
            else {
                return Err(CompileError::TypeMismatch);
            };
            let enum_type = context
                .enum_types
                .iter()
                .find(|enum_type| enum_type.type_id == type_id)
                .ok_or(CompileError::TypeMismatch)?;
            let mut covered = BTreeSet::new();
            let mut result_type = expected;
            for arm in arms {
                let variant = context
                    .enum_variants
                    .get(&(type_id, arm.variant.clone()))
                    .ok_or(CompileError::TypeMismatch)?;
                if !covered.insert(variant.stable_id) {
                    return Err(CompileError::DuplicateMatchVariant);
                }
                match (&arm.binding, variant.payload_type) {
                    (Some(binding), Some(payload_type)) => {
                        if !locals.contains_key(binding) {
                            let register = *next_register;
                            *next_register = next_register
                                .checked_add(1)
                                .ok_or(CompileError::TooManyRegisters)?;
                            locals.insert(binding.clone(), (register, payload_type));
                        }
                    }
                    (None, None) => {}
                    _ => return Err(CompileError::TypeMismatch),
                }
                let arm_type =
                    expression_type(&arm.value, locals, context, next_register, result_type)?;
                if let Some(expected) = result_type
                    && arm_type != expected
                {
                    return Err(CompileError::TypeMismatch);
                }
                result_type = Some(arm_type);
            }
            if covered.len() != enum_type.variants.len() {
                return Err(CompileError::NonExhaustiveMatch);
            }
            result_type.ok_or(CompileError::NonExhaustiveMatch)?
        }
        AstExpression::Try(expression) => {
            let ValueType::Named(type_id) =
                expression_type(expression, locals, context, next_register, None)?
            else {
                return Err(CompileError::TypeMismatch);
            };
            let ok = context
                .enum_variants
                .get(&(type_id, "Ok".to_owned()))
                .ok_or(CompileError::TypeMismatch)?;
            let error = context
                .enum_variants
                .get(&(type_id, "Err".to_owned()))
                .ok_or(CompileError::TypeMismatch)?;
            let ValueType::Named(function_result) = context.function_result else {
                return Err(CompileError::TypeMismatch);
            };
            let result_error = context
                .enum_variants
                .get(&(function_result, "Err".to_owned()))
                .ok_or(CompileError::TypeMismatch)?;
            if error.payload_type != result_error.payload_type {
                return Err(CompileError::TypeMismatch);
            }
            ok.payload_type.ok_or(CompileError::TypeMismatch)?
        }
        AstExpression::Migration(intrinsic) => {
            if context.effect != FunctionEffect::Migration {
                return Err(CompileError::InvalidEffect);
            }
            migration_intrinsic_type(intrinsic, locals, context, next_register)?
        }
        AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
    };
    if let Some(expected) = expected
        && actual != expected
    {
        if matches!(
            (actual, expected),
            (ValueType::I64, ValueType::I32) | (ValueType::F64, ValueType::F32)
        ) {
            return Err(CompileError::InvalidNumericConversion {
                span: expression.span(),
            });
        }
        return Err(CompileError::TypeMismatch);
    }
    Ok(actual)
}

fn migration_intrinsic_type(
    intrinsic: &MigrationIntrinsic,
    locals: &mut BTreeMap<String, (u16, ValueType)>,
    context: &TypeContext<'_>,
    next_register: &mut u16,
) -> Result<ValueType, CompileError> {
    match intrinsic.kind() {
        MigrationIntrinsic::OldGet { ty, .. }
        | MigrationIntrinsic::NewCreate { ty, .. }
        | MigrationIntrinsic::OldFieldGet { ty, .. } => {
            if let MigrationIntrinsic::OldFieldGet { object, owner, .. } = intrinsic.kind()
                && expression_type(
                    object,
                    locals,
                    context,
                    next_register,
                    Some(ValueType::Named(StableId::from_name(owner))),
                )? != ValueType::Named(StableId::from_name(owner))
            {
                return Err(CompileError::TypeMismatch);
            }
            Ok(lower_type(ty))
        }
        MigrationIntrinsic::NewSet {
            object,
            owner,
            field,
            value,
        } => {
            let owner_type = StableId::from_name(owner);
            if expression_type(
                object,
                locals,
                context,
                next_register,
                Some(ValueType::Named(owner_type)),
            )? != ValueType::Named(owner_type)
            {
                return Err(CompileError::TypeMismatch);
            }
            let field_id = StableId::from_parts(&[owner, "::", field]);
            let expected = context
                .state_schema
                .types
                .iter()
                .find(|state_type| state_type.stable_id == owner_type)
                .and_then(|state_type| {
                    state_type
                        .fields
                        .iter()
                        .find(|candidate| candidate.stable_id == field_id)
                })
                .map(|field| field.ty)
                .ok_or(CompileError::TypeMismatch)?;
            if expression_type(value, locals, context, next_register, Some(expected))? != expected {
                return Err(CompileError::TypeMismatch);
            }
            Ok(ValueType::Bool)
        }
        MigrationIntrinsic::Replace { target, .. } => {
            let target_type = expression_type(target, locals, context, next_register, None)?;
            if !target_type.is_reference() {
                return Err(CompileError::TypeMismatch);
            }
            Ok(ValueType::Bool)
        }
        MigrationIntrinsic::Preserve { .. }
        | MigrationIntrinsic::Delete { .. }
        | MigrationIntrinsic::Finish => Ok(ValueType::Bool),
        MigrationIntrinsic::Spanned { .. } => unreachable!("kind strips spans"),
    }
}

fn lower_type(ty: &AstType) -> ValueType {
    match ty.kind() {
        AstType::I32 => ValueType::I32,
        AstType::I64 => ValueType::I64,
        AstType::F32 => ValueType::F32,
        AstType::F64 => ValueType::F64,
        AstType::Bool => ValueType::Bool,
        AstType::Rune => ValueType::Rune,
        AstType::String => ValueType::String,
        AstType::Named(name) => ValueType::Named(StableId::from_name(name)),
        AstType::BuiltinGeneric { name, arguments } if name == "Option" => {
            ValueType::Named(nexa_bytecode::option_type(lower_type(&arguments[0])).type_id)
        }
        AstType::BuiltinGeneric { name, arguments } if name == "Result" => ValueType::Named(
            nexa_bytecode::result_type(lower_type(&arguments[0]), lower_type(&arguments[1]))
                .type_id,
        ),
        AstType::BuiltinGeneric { name, arguments } if name == "StateHandle" => {
            ValueType::Named(nexa_bytecode::state_handle_type(lower_type(&arguments[0])))
        }
        AstType::BuiltinGeneric { name, arguments } if name == "Array" => {
            ValueType::Named(nexa_bytecode::array_type(lower_type(&arguments[0])))
        }
        AstType::BuiltinGeneric { name, arguments } if name == "Map" => ValueType::Named(
            nexa_bytecode::map_type(lower_type(&arguments[0]), lower_type(&arguments[1])),
        ),
        AstType::BuiltinGeneric { name, arguments } if name == "Buffer" => {
            ValueType::Named(nexa_bytecode::buffer_type(lower_type(&arguments[0])))
        }
        AstType::BuiltinGeneric { .. } => unreachable!("generic types are validated"),
        AstType::Spanned { .. } => unreachable!("kind strips spans"),
    }
}

fn plan_registers(function: &HirFunction) -> Result<RegisterPlan, CompileError> {
    let local_count = function
        .locals
        .values()
        .map(|(register, _)| register.saturating_add(1))
        .max()
        .unwrap_or_else(|| u16::try_from(function.signature.parameters.len()).unwrap_or(u16::MAX));
    let mut plan = RegisterPlan {
        local_count,
        expression_temporaries: 1,
        ..RegisterPlan::default()
    };
    inspect_statement_registers(&function.body, &mut plan)?;
    let temporary_count = plan
        .expression_temporaries
        .max(plan.max_call_arguments)
        .max(plan.match_temporaries)
        .max(plan.migration_temporaries)
        .max(1);
    plan.total = plan
        .local_count
        .checked_add(temporary_count)
        .ok_or(CompileError::TooManyRegisters)?;
    Ok(plan)
}

fn inspect_statement_registers(
    statements: &[AstStatement],
    plan: &mut RegisterPlan,
) -> Result<(), CompileError> {
    for statement in statements {
        match statement.kind() {
            AstStatement::Bind { value, .. }
            | AstStatement::Return(value)
            | AstStatement::Expression(value)
            | AstStatement::Defer(value) => inspect_expression_registers(value, plan)?,
            AstStatement::FieldSet {
                value, replacement, ..
            } => {
                inspect_expression_registers(value, plan)?;
                inspect_expression_registers(replacement, plan)?;
                plan.expression_temporaries = plan.expression_temporaries.max(
                    temporary_requirement(replacement)?
                        .checked_add(1)
                        .ok_or(CompileError::TooManyRegisters)?,
                );
            }
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                inspect_expression_registers(condition, plan)?;
                inspect_statement_registers(then_body, plan)?;
                inspect_statement_registers(else_body, plan)?;
            }
            AstStatement::While { condition, body } => {
                inspect_expression_registers(condition, plan)?;
                inspect_statement_registers(body, plan)?;
            }
            AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
        }
    }
    Ok(())
}

fn inspect_expression_registers(
    expression: &AstExpression,
    plan: &mut RegisterPlan,
) -> Result<(), CompileError> {
    let requirement = temporary_requirement(expression)?;
    plan.expression_temporaries = plan.expression_temporaries.max(requirement);
    match expression.kind() {
        AstExpression::Binary { lhs, rhs, .. } => {
            inspect_expression_registers(lhs, plan)?;
            inspect_expression_registers(rhs, plan)?;
        }
        AstExpression::Call { arguments, .. } => {
            let window = u16::try_from(arguments.len())
                .map_err(|_| CompileError::TooManyRegisters)?
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            plan.max_call_arguments = plan.max_call_arguments.max(window);
            for argument in arguments {
                inspect_expression_registers(argument, plan)?;
            }
        }
        AstExpression::Await(expression) | AstExpression::Try(expression) => {
            inspect_expression_registers(expression, plan)?;
        }
        AstExpression::Constructor { payload, .. } => {
            if let Some(payload) = payload {
                inspect_expression_registers(payload, plan)?;
            }
        }
        AstExpression::StructLiteral { fields, .. } | AstExpression::ClassNew { fields, .. } => {
            for field in fields {
                inspect_expression_registers(&field.value, plan)?;
            }
        }
        AstExpression::FieldGet { value, .. } => {
            inspect_expression_registers(value, plan)?;
        }
        AstExpression::StructWith { value, updates } => {
            inspect_expression_registers(value, plan)?;
            for field in updates {
                inspect_expression_registers(&field.value, plan)?;
            }
        }
        AstExpression::Match { value, arms } => {
            plan.match_temporaries = plan.match_temporaries.max(requirement);
            inspect_expression_registers(value, plan)?;
            for arm in arms {
                inspect_expression_registers(&arm.value, plan)?;
            }
        }
        AstExpression::Migration(intrinsic) => {
            plan.migration_temporaries = plan.migration_temporaries.max(requirement);
            for expression in migration_expressions(intrinsic) {
                inspect_expression_registers(expression, plan)?;
            }
        }
        AstExpression::Integer(_)
        | AstExpression::Float(_)
        | AstExpression::Rune(_)
        | AstExpression::String(_)
        | AstExpression::Bool(_)
        | AstExpression::ArrayNew { .. }
        | AstExpression::MapNew { .. }
        | AstExpression::Name(_) => {}
        AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
    }
    Ok(())
}

fn temporary_requirement(expression: &AstExpression) -> Result<u16, CompileError> {
    let offset_requirement =
        |offset: usize, nested: &AstExpression| -> Result<usize, CompileError> {
            offset
                .checked_add(usize::from(temporary_requirement(nested)?))
                .ok_or(CompileError::TooManyRegisters)
        };
    let required = match expression.kind() {
        AstExpression::Integer(_)
        | AstExpression::Float(_)
        | AstExpression::Rune(_)
        | AstExpression::String(_)
        | AstExpression::Bool(_)
        | AstExpression::ArrayNew { .. }
        | AstExpression::MapNew { .. }
        | AstExpression::Name(_) => 1,
        AstExpression::Binary { lhs, rhs, .. } => {
            usize::from(temporary_requirement(lhs)?).max(offset_requirement(1, rhs)?)
        }
        AstExpression::Call { arguments, .. } => {
            let mut required = 1;
            for (index, argument) in arguments.iter().enumerate() {
                required = required.max(offset_requirement(index + 1, argument)?);
            }
            required
        }
        AstExpression::Await(expression) => usize::from(temporary_requirement(expression)?),
        AstExpression::Constructor { payload, .. } => payload
            .as_deref()
            .map_or(Ok(1), |payload| offset_requirement(1, payload))?,
        AstExpression::StructLiteral { fields, .. } | AstExpression::ClassNew { fields, .. } => {
            let mut required = 1;
            for (index, field) in fields.iter().enumerate() {
                required = required.max(offset_requirement(index + 1, &field.value)?);
            }
            required
        }
        AstExpression::FieldGet { value, .. } => usize::from(temporary_requirement(value)?),
        AstExpression::StructWith { value, updates } => {
            let mut required = usize::from(temporary_requirement(value)?);
            for field in updates {
                required = required.max(offset_requirement(1, &field.value)?);
            }
            required
        }
        AstExpression::Match { value, arms } => {
            let mut required = usize::from(temporary_requirement(value)?).max(4);
            for arm in arms {
                required = required.max(usize::from(temporary_requirement(&arm.value)?));
            }
            required
        }
        AstExpression::Try(expression) => usize::from(temporary_requirement(expression)?).max(4),
        AstExpression::Migration(intrinsic) => match intrinsic.kind() {
            MigrationIntrinsic::OldGet { .. }
            | MigrationIntrinsic::NewCreate { .. }
            | MigrationIntrinsic::Preserve { .. }
            | MigrationIntrinsic::Delete { .. }
            | MigrationIntrinsic::Finish => 1,
            MigrationIntrinsic::OldFieldGet { object, .. } => offset_requirement(1, object)?.max(1),
            MigrationIntrinsic::NewSet { object, value, .. } => {
                usize::from(temporary_requirement(object)?).max(offset_requirement(1, value)?)
            }
            MigrationIntrinsic::Replace { target, .. } => {
                usize::from(temporary_requirement(target)?)
            }
            MigrationIntrinsic::Spanned { .. } => unreachable!("kind strips spans"),
        },
        AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
    };
    u16::try_from(required).map_err(|_| CompileError::TooManyRegisters)
}

fn collect_string_literals(statements: &[AstStatement], strings: &mut BTreeSet<String>) {
    for statement in statements {
        match statement.kind() {
            AstStatement::Bind { value, .. }
            | AstStatement::Return(value)
            | AstStatement::Expression(value)
            | AstStatement::Defer(value) => collect_expression_strings(value, strings),
            AstStatement::FieldSet {
                value, replacement, ..
            } => {
                collect_expression_strings(value, strings);
                collect_expression_strings(replacement, strings);
            }
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                collect_expression_strings(condition, strings);
                collect_string_literals(then_body, strings);
                collect_string_literals(else_body, strings);
            }
            AstStatement::While { condition, body } => {
                collect_expression_strings(condition, strings);
                collect_string_literals(body, strings);
            }
            AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
        }
    }
}

fn collect_expression_strings(expression: &AstExpression, strings: &mut BTreeSet<String>) {
    match expression.kind() {
        AstExpression::String(value) => {
            strings.insert(value.clone());
        }
        AstExpression::Binary { lhs, rhs, .. } => {
            collect_expression_strings(lhs, strings);
            collect_expression_strings(rhs, strings);
        }
        AstExpression::Call { arguments, .. } => {
            for argument in arguments {
                collect_expression_strings(argument, strings);
            }
        }
        AstExpression::Await(expression) | AstExpression::Try(expression) => {
            collect_expression_strings(expression, strings);
        }
        AstExpression::Constructor { payload, .. } => {
            if let Some(payload) = payload {
                collect_expression_strings(payload, strings);
            }
        }
        AstExpression::StructLiteral { fields, .. } | AstExpression::ClassNew { fields, .. } => {
            for field in fields {
                collect_expression_strings(&field.value, strings);
            }
        }
        AstExpression::FieldGet { value, .. } => collect_expression_strings(value, strings),
        AstExpression::StructWith { value, updates } => {
            collect_expression_strings(value, strings);
            for field in updates {
                collect_expression_strings(&field.value, strings);
            }
        }
        AstExpression::Match { value, arms } => {
            collect_expression_strings(value, strings);
            for arm in arms {
                collect_expression_strings(&arm.value, strings);
            }
        }
        AstExpression::Migration(intrinsic) => {
            for expression in migration_expressions(intrinsic) {
                collect_expression_strings(expression, strings);
            }
        }
        AstExpression::Integer(_)
        | AstExpression::Float(_)
        | AstExpression::Rune(_)
        | AstExpression::Bool(_)
        | AstExpression::ArrayNew { .. }
        | AstExpression::MapNew { .. }
        | AstExpression::Name(_) => {}
        AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
    }
}

pub fn emit_bytecode(hir: &HirModule) -> Result<Module, CompileError> {
    let function_ids = hir
        .functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            (
                function.name.clone(),
                u32::try_from(index).expect("function count is bounded by registers"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut module = ModuleBuilder::new();
    let mut string_literals = BTreeSet::new();
    for function in &hir.functions {
        collect_string_literals(&function.body, &mut string_literals);
    }
    let string_indices = string_literals
        .into_iter()
        .map(|value| {
            let index = module.string(value.clone());
            (value, index)
        })
        .collect::<BTreeMap<_, _>>();
    let mut host_functions = hir.host_functions.values().collect::<Vec<_>>();
    host_functions.sort_by_key(|function| function.import);
    for function in host_functions {
        module.host_import(function.metadata.clone());
    }
    emit_module_metadata(hir, &mut module);
    let mut source_map = Vec::new();
    for (function_index, function) in hir.functions.iter().enumerate() {
        let mut code = TrackedCode::new(function.span);
        let register_plan = plan_registers(function)?;
        let temporary = register_plan.local_count;
        let emit_context = EmitContext {
            functions: &function_ids,
            script_functions: &hir.functions,
            host_functions: &hir.host_functions,
            enum_variants: &hir.enum_variants,
            struct_types: &hir.struct_types,
            struct_fields: &hir.struct_fields,
            class_types: &hir.class_types,
            class_fields: &hir.class_fields,
            state_schema: &hir.state_schema,
            function_result: function.signature.result.expect("result is required"),
            state_handle_targets: &hir.state_handle_targets,
            array_types: &hir.array_types,
            map_types: &hir.map_types,
            buffer_types: &hir.buffer_types,
            string_indices: &string_indices,
        };
        emit_statements(
            &function.body,
            temporary,
            &function.locals,
            &emit_context,
            &mut code,
        )?;
        let registers = register_plan.total;
        let safepoints = collect_safepoints(&code.instructions);
        let (root_bitmap, root_maps) =
            exact_root_maps(function, &code.instructions, &safepoints, hir, registers)?;
        source_map.extend(
            code.entries
                .iter()
                .map(|(pc_start, pc_end, span)| SourceMapEntry {
                    function: u32::try_from(function_index)
                        .expect("function count is compiler bounded"),
                    pc_start: *pc_start,
                    pc_end: *pc_end,
                    span: *span,
                }),
        );
        module.function(Function {
            signature: function.signature.clone(),
            registers,
            frame_bytes: u32::from(registers) * 8,
            root_bitmap,
            root_maps,
            safepoints,
            loop_bounds: Vec::new(),
            effect: function.effect,
            max_static_call_depth: 1,
            code: code.instructions,
        });
    }
    module.source_map(source_map);
    let mut module = module.finish();
    finalize_reload_metadata(hir, &mut module);
    Ok(module)
}

fn finalize_reload_metadata(hir: &HirModule, module: &mut Module) {
    module.reload_metadata.migration_entry = hir
        .functions
        .iter()
        .position(|function| function.effect == FunctionEffect::Migration)
        .map(|entry| u32::try_from(entry).expect("function count is compiler bounded"));
    module.reload_metadata.activation_entry = hir
        .functions
        .iter()
        .position(|function| function.is_activation)
        .map(|entry| u32::try_from(entry).expect("function count is compiler bounded"));
    module.reload_metadata.minimum_migration_limits =
        nexa_bytecode::minimum_migration_limits(module, module.reload_metadata.migration_entry);
}

fn emit_module_metadata(hir: &HirModule, module: &mut ModuleBuilder) {
    if let (Some(host), Some(schema)) = (hir.host_interface_hash, hir.schema_hash) {
        module.metadata(host, schema);
    }
    module.state_schema(hir.state_schema.clone());
    for (type_id, target) in &hir.state_handle_targets {
        module.state_handle_type(StateHandleType {
            type_id: *type_id,
            target: *target,
        });
    }
    for array_type in &hir.array_types {
        module.array_type(*array_type);
    }
    for map_type in &hir.map_types {
        module.map_type(*map_type);
    }
    for buffer_type in &hir.buffer_types {
        module.buffer_type(*buffer_type);
    }
    for enum_type in &hir.enum_types {
        module.enum_type(enum_type.clone());
    }
    for struct_type in &hir.struct_types {
        module.struct_type(struct_type.clone());
    }
    for class_type in &hir.class_types {
        module.class_type(class_type.clone());
    }
}

#[allow(clippy::too_many_lines)]
fn exact_root_maps(
    function: &HirFunction,
    code: &[Instruction],
    safepoints: &[u32],
    module: &HirModule,
    registers: u16,
) -> Result<(Vec<bool>, Vec<RootMap>), CompileError> {
    use std::collections::VecDeque;

    let register_count = usize::from(registers);
    let mut entry = vec![None; register_count];
    for (index, ty) in function.signature.parameters.iter().copied().enumerate() {
        entry[index] = Some(ty);
    }
    let mut states = vec![None; code.len()];
    if !code.is_empty() {
        states[0] = Some(entry);
    }
    let mut queue = VecDeque::from([0_usize]);
    while let Some(pc) = queue.pop_front() {
        let Some(mut state) = states[pc].clone() else {
            continue;
        };
        let mut successors = Vec::with_capacity(2);
        match code[pc] {
            Instruction::LoadI32 { dst, .. }
            | Instruction::Add { dst, .. }
            | Instruction::Sub { dst, .. }
            | Instruction::Mul { dst, .. }
            | Instruction::Div { dst, .. } => state[usize::from(dst)] = Some(ValueType::I32),
            Instruction::LoadI64 { dst, .. }
            | Instruction::AddI64 { dst, .. }
            | Instruction::SubI64 { dst, .. }
            | Instruction::MulI64 { dst, .. }
            | Instruction::DivI64 { dst, .. }
            | Instruction::StringHash { dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::I64);
            }
            Instruction::LoadF32 { dst, .. }
            | Instruction::AddF32 { dst, .. }
            | Instruction::SubF32 { dst, .. }
            | Instruction::MulF32 { dst, .. }
            | Instruction::DivF32 { dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::F32);
            }
            Instruction::LoadF64 { dst, .. }
            | Instruction::AddF64 { dst, .. }
            | Instruction::SubF64 { dst, .. }
            | Instruction::MulF64 { dst, .. }
            | Instruction::DivF64 { dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::F64);
            }
            Instruction::LoadRune { dst, .. } | Instruction::StringRuneAt { dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::Rune);
            }
            Instruction::LoadString { dst, .. } | Instruction::StringConcat { dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::String);
            }
            Instruction::LoadBool { dst, .. }
            | Instruction::CompareEq { dst, .. }
            | Instruction::StringEqual { dst, .. }
            | Instruction::StructEqual { dst, .. }
            | Instruction::ClassEqual { dst, .. }
            | Instruction::MapContains { dst, .. }
            | Instruction::StateHandleIsAlive { dst, .. }
            | Instruction::StateHandleEqual { dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::Bool);
            }
            Instruction::Move { dst, source }
            | Instruction::StructWith { source, dst, .. }
            | Instruction::BufferSlice { source, dst, .. } => {
                state[usize::from(dst)] = state[usize::from(source)];
            }
            Instruction::Call {
                function: callee,
                dst,
                ..
            } => {
                state[usize::from(dst)] = module.functions[callee as usize].signature.result;
            }
            Instruction::HostCall { import, dst, .. } => {
                state[usize::from(dst)] = module
                    .host_functions
                    .values()
                    .find(|function| function.import == import)
                    .and_then(|function| function.metadata.result);
            }
            Instruction::StateOldGet { ty, dst, .. }
            | Instruction::StateOldFieldGet { ty, dst, .. } => {
                state[usize::from(dst)] = Some(ty);
            }
            Instruction::StateNewCreate { type_id, dst, .. }
            | Instruction::EnumNew { type_id, dst, .. }
            | Instruction::StructNew { type_id, dst, .. }
            | Instruction::ClassNew { type_id, dst, .. }
            | Instruction::ArrayNew { type_id, dst }
            | Instruction::MapNew { type_id, dst } => {
                state[usize::from(dst)] = Some(ValueType::Named(type_id));
            }
            Instruction::StateHandleResolve {
                result_type, dst, ..
            } => state[usize::from(dst)] = Some(ValueType::Named(result_type)),
            Instruction::StateHandleStableId { dst, .. } => {
                state[usize::from(dst)] = Some(nexa_bytecode::stable_id_type());
            }
            Instruction::StateHandleGeneration { dst, .. }
            | Instruction::StateHandleHash { dst, .. }
            | Instruction::StringLen { dst, .. }
            | Instruction::StringByteLen { dst, .. }
            | Instruction::ArrayLen { dst, .. }
            | Instruction::MapLen { dst, .. }
            | Instruction::BufferLen { dst, .. }
            | Instruction::EnumTag { dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::I32);
            }
            Instruction::EnumPayload {
                source,
                variant,
                dst,
            } => {
                let Some(ValueType::Named(type_id)) = state[usize::from(source)] else {
                    return Err(CompileError::TypeMismatch);
                };
                state[usize::from(dst)] = module
                    .enum_types
                    .iter()
                    .find(|enum_type| enum_type.type_id == type_id)
                    .and_then(|enum_type| {
                        enum_type
                            .variants
                            .iter()
                            .find(|candidate| candidate.stable_id == variant)
                    })
                    .and_then(|variant| variant.payload_type);
            }
            Instruction::StructGet { source, field, dst } => {
                let Some(ValueType::Named(type_id)) = state[usize::from(source)] else {
                    return Err(CompileError::TypeMismatch);
                };
                state[usize::from(dst)] = module
                    .struct_types
                    .iter()
                    .find(|struct_type| struct_type.type_id == type_id)
                    .and_then(|struct_type| {
                        struct_type
                            .fields
                            .iter()
                            .find(|candidate| candidate.stable_id == field)
                    })
                    .map(|field| field.ty);
            }
            Instruction::ClassGet { source, field, dst } => {
                let Some(ValueType::Named(type_id)) = state[usize::from(source)] else {
                    return Err(CompileError::TypeMismatch);
                };
                state[usize::from(dst)] = module
                    .class_types
                    .iter()
                    .find(|class_type| class_type.type_id == type_id)
                    .and_then(|class_type| {
                        class_type
                            .fields
                            .iter()
                            .find(|candidate| candidate.stable_id == field)
                    })
                    .map(|field| field.ty);
            }
            Instruction::ArrayGet { source, dst, .. }
            | Instruction::ArrayPop { source, dst }
            | Instruction::ArrayRemove { source, dst, .. } => {
                let Some(ValueType::Named(type_id)) = state[usize::from(source)] else {
                    return Err(CompileError::TypeMismatch);
                };
                state[usize::from(dst)] = module
                    .array_types
                    .iter()
                    .find(|array_type| array_type.type_id == type_id)
                    .map(|array_type| array_type.element);
            }
            Instruction::MapGet {
                result_type, dst, ..
            }
            | Instruction::MapRemove {
                result_type, dst, ..
            } => {
                state[usize::from(dst)] = Some(ValueType::Named(result_type));
            }
            Instruction::BufferGet { source, dst, .. } => {
                let Some(ValueType::Named(type_id)) = state[usize::from(source)] else {
                    return Err(CompileError::TypeMismatch);
                };
                state[usize::from(dst)] = module
                    .buffer_types
                    .iter()
                    .find(|buffer| buffer.type_id == type_id)
                    .map(|buffer| buffer.element);
            }
            Instruction::Jump { target } => successors.push(target as usize),
            Instruction::JumpIfFalse { target, .. } => {
                successors.push(target as usize);
                if pc + 1 < code.len() {
                    successors.push(pc + 1);
                }
            }
            Instruction::DeferPush { .. }
            | Instruction::StateNewSet { .. }
            | Instruction::StateReplace { .. }
            | Instruction::StateDelete { .. }
            | Instruction::StatePreserve { .. }
            | Instruction::StateFinish
            | Instruction::ClassSet { .. }
            | Instruction::ArraySet { .. }
            | Instruction::ArrayPush { .. }
            | Instruction::ArrayInsert { .. }
            | Instruction::ArrayClear { .. }
            | Instruction::MapSet { .. }
            | Instruction::MapClear { .. }
            | Instruction::BufferSet { .. }
            | Instruction::BufferCopy { .. }
            | Instruction::DeferPop
            | Instruction::CleanupReturn
            | Instruction::Return { .. }
            | Instruction::ReturnVoid
            | Instruction::Safepoint
            | Instruction::Yield
            | Instruction::Trap => {}
        }
        if !matches!(
            code[pc],
            Instruction::Jump { .. }
                | Instruction::Return { .. }
                | Instruction::ReturnVoid
                | Instruction::CleanupReturn
                | Instruction::Trap
        ) && successors.is_empty()
            && pc + 1 < code.len()
        {
            successors.push(pc + 1);
        }
        for successor in successors {
            if successor >= states.len() {
                return Err(CompileError::Verify(
                    "emitter produced an out-of-range control-flow target".into(),
                ));
            }
            match &mut states[successor] {
                None => {
                    states[successor] = Some(state.clone());
                    queue.push_back(successor);
                }
                Some(existing) => {
                    let mut changed = false;
                    for (current, incoming) in existing.iter_mut().zip(&state) {
                        if *current != *incoming && current.take().is_some() {
                            changed = true;
                        }
                    }
                    if changed {
                        queue.push_back(successor);
                    }
                }
            }
        }
    }
    let root_bitmap = (0..register_count)
        .map(|register| {
            states
                .iter()
                .flatten()
                .any(|state| state[register].is_some_and(ValueType::is_reference))
        })
        .collect();
    let root_maps = safepoints
        .iter()
        .map(|pc| RootMap {
            pc: *pc,
            bitmap: states[*pc as usize].as_ref().map_or_else(
                || vec![false; register_count],
                |state| {
                    state
                        .iter()
                        .map(|ty| ty.is_some_and(ValueType::is_reference))
                        .collect()
                },
            ),
        })
        .collect();
    Ok((root_bitmap, root_maps))
}

struct EmitContext<'a> {
    functions: &'a BTreeMap<String, u32>,
    script_functions: &'a [HirFunction],
    host_functions: &'a BTreeMap<String, HostFunction>,
    enum_variants: &'a BTreeMap<(StableId, String), EnumVariant>,
    struct_types: &'a [StructType],
    struct_fields: &'a BTreeMap<(StableId, String), BytecodeStructField>,
    class_types: &'a [ClassType],
    class_fields: &'a BTreeMap<(StableId, String), BytecodeStructField>,
    state_schema: &'a StateSchema,
    function_result: ValueType,
    state_handle_targets: &'a BTreeMap<StableId, ValueType>,
    array_types: &'a [ArrayType],
    map_types: &'a [MapType],
    buffer_types: &'a [BufferType],
    string_indices: &'a BTreeMap<String, u32>,
}

struct TrackedCode {
    instructions: Vec<Instruction>,
    entries: Vec<(u32, u32, SourceSpan)>,
    span: SourceSpan,
    pending_next: Vec<SourceSpan>,
}

impl TrackedCode {
    fn new(span: SourceSpan) -> Self {
        Self {
            instructions: Vec::new(),
            entries: Vec::new(),
            span,
            pending_next: Vec::new(),
        }
    }

    fn replace_span(&mut self, span: SourceSpan) -> SourceSpan {
        std::mem::replace(&mut self.span, span)
    }

    fn push(&mut self, instruction: Instruction) {
        let start =
            u32::try_from(self.instructions.len()).expect("instruction count is compiler bounded");
        self.instructions.push(instruction);
        self.entries
            .push((start, start.saturating_add(1), self.span));
        self.entries.extend(
            self.pending_next
                .drain(..)
                .map(|span| (start, start.saturating_add(1), span)),
        );
    }

    fn map_next(&mut self, span: SourceSpan) {
        self.pending_next.push(span);
    }
}

impl Deref for TrackedCode {
    type Target = Vec<Instruction>;

    fn deref(&self) -> &Self::Target {
        &self.instructions
    }
}

impl DerefMut for TrackedCode {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.instructions
    }
}

#[allow(clippy::too_many_lines)]
fn emit_statements(
    statements: &[AstStatement],
    temporary: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    for statement in statements {
        let previous_span = code.replace_span(statement.span());
        match statement.kind() {
            AstStatement::Bind { name, value, .. } => {
                emit_expression(
                    value,
                    temporary,
                    Some(locals[name].1),
                    locals,
                    context,
                    code,
                )?;
                code.push(Instruction::Move {
                    dst: locals[name].0,
                    source: temporary,
                });
            }
            AstStatement::Return(value) => {
                emit_expression(
                    value,
                    temporary,
                    Some(context.function_result),
                    locals,
                    context,
                    code,
                )?;
                code.push(Instruction::Return { source: temporary });
            }
            AstStatement::Expression(expression) => {
                emit_expression(expression, temporary, None, locals, context, code)?;
            }
            AstStatement::Defer(expression) => {
                let AstExpression::Call {
                    function,
                    arguments,
                } = expression.kind()
                else {
                    return Err(CompileError::SuspendingDefer);
                };
                if arguments.len() > 8 {
                    return Err(CompileError::DeferCaptureLimit);
                }
                let args_base = temporary
                    .checked_add(1)
                    .ok_or(CompileError::TooManyRegisters)?;
                for (index, argument) in arguments.iter().enumerate() {
                    emit_expression(
                        argument,
                        args_base
                            .checked_add(
                                u16::try_from(index).map_err(|_| CompileError::TooManyRegisters)?,
                            )
                            .ok_or(CompileError::TooManyRegisters)?,
                        None,
                        locals,
                        context,
                        code,
                    )?;
                }
                code.push(Instruction::DeferPush {
                    function: *context
                        .functions
                        .get(function)
                        .ok_or_else(|| CompileError::UnknownName(function.clone()))?,
                    args_base,
                    args_count: u16::try_from(arguments.len())
                        .map_err(|_| CompileError::TooManyRegisters)?,
                });
            }
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                emit_expression(
                    condition,
                    temporary,
                    Some(ValueType::Bool),
                    locals,
                    context,
                    code,
                )?;
                let branch = code.len();
                code.push(Instruction::JumpIfFalse {
                    condition: temporary,
                    target: 0,
                });
                emit_statements(then_body, temporary, locals, context, code)?;
                let skip_else = code.len();
                code.push(Instruction::Jump { target: 0 });
                let else_start =
                    u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
                code[branch] = Instruction::JumpIfFalse {
                    condition: temporary,
                    target: else_start,
                };
                emit_statements(else_body, temporary, locals, context, code)?;
                let end = u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
                code.push(Instruction::Safepoint);
                code[skip_else] = Instruction::Jump { target: end };
            }
            AstStatement::While { condition, body } => {
                let loop_start =
                    u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
                code.push(Instruction::Safepoint);
                emit_expression(
                    condition,
                    temporary,
                    Some(ValueType::Bool),
                    locals,
                    context,
                    code,
                )?;
                let exit = code.len();
                code.push(Instruction::JumpIfFalse {
                    condition: temporary,
                    target: 0,
                });
                emit_statements(body, temporary, locals, context, code)?;
                code.push(Instruction::Jump { target: loop_start });
                let end = u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
                code.push(Instruction::Safepoint);
                code[exit] = Instruction::JumpIfFalse {
                    condition: temporary,
                    target: end,
                };
            }
            AstStatement::FieldSet {
                value,
                field,
                replacement,
            } => {
                let ValueType::Named(type_id) =
                    emitted_expression_type(value, None, locals, context)?
                else {
                    return Err(CompileError::TypeMismatch);
                };
                let metadata = context
                    .class_fields
                    .get(&(type_id, field.clone()))
                    .ok_or(CompileError::TypeMismatch)?;
                emit_expression(
                    value,
                    temporary,
                    Some(ValueType::Named(type_id)),
                    locals,
                    context,
                    code,
                )?;
                let replacement_register = temporary
                    .checked_add(1)
                    .ok_or(CompileError::TooManyRegisters)?;
                emit_expression(
                    replacement,
                    replacement_register,
                    Some(metadata.ty),
                    locals,
                    context,
                    code,
                )?;
                code.push(Instruction::ClassSet {
                    source: temporary,
                    field: metadata.stable_id,
                    value: replacement_register,
                });
            }
            AstStatement::Spanned { .. } => unreachable!("kind strips spans"),
        }
        code.replace_span(previous_span);
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
fn emit_expression(
    expression: &AstExpression,
    destination: u16,
    expected: Option<ValueType>,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    let expression_span = expression.span();
    let previous_span = code.replace_span(expression_span);
    if let AstExpression::Call {
        function,
        arguments,
    } = expression.kind()
        && string_method(function).is_some_and(|(receiver, _)| {
            locals.get(receiver).map(|(_, ty)| *ty) == Some(ValueType::String)
        })
    {
        let result = emit_string_method(function, arguments, destination, locals, context, code);
        code.replace_span(previous_span);
        return result;
    }
    if let AstExpression::Call {
        function,
        arguments,
    } = expression.kind()
        && buffer_method(function).is_some_and(|(receiver, _)| {
            locals.get(receiver).is_some_and(|(_, ty)| {
                matches!(ty, ValueType::Named(type_id) if context
                    .buffer_types
                    .iter()
                    .any(|buffer| buffer.type_id == *type_id))
            })
        })
    {
        let result = emit_buffer_method(function, arguments, destination, locals, context, code);
        code.replace_span(previous_span);
        return result;
    }
    if let AstExpression::Call {
        function,
        arguments,
    } = expression.kind()
        && map_method(function).is_some_and(|(receiver, _)| {
            locals.get(receiver).is_some_and(|(_, ty)| {
                matches!(ty, ValueType::Named(type_id) if context
                    .map_types
                    .iter()
                    .any(|map_type| map_type.type_id == *type_id))
            })
        })
    {
        let result = emit_map_method(function, arguments, destination, locals, context, code);
        code.replace_span(previous_span);
        return result;
    }
    if let AstExpression::Call {
        function,
        arguments,
    } = expression.kind()
        && array_method(function).is_some()
    {
        let result = emit_array_method(function, arguments, destination, locals, context, code);
        code.replace_span(previous_span);
        return result;
    }
    if let AstExpression::Call {
        function,
        arguments,
    } = expression.kind()
        && state_handle_method(function).is_some()
    {
        let result =
            emit_state_handle_method(function, arguments, destination, locals, context, code);
        code.replace_span(previous_span);
        return result;
    }
    match expression.kind() {
        AstExpression::Integer(value) => match expected {
            Some(ValueType::I64) => code.push(Instruction::LoadI64 {
                dst: destination,
                value: *value,
            }),
            Some(ValueType::I32) | None => code.push(Instruction::LoadI32 {
                dst: destination,
                value: i32::try_from(*value).map_err(|_| {
                    CompileError::InvalidNumericConversion {
                        span: expression.span(),
                    }
                })?,
            }),
            Some(_) => return Err(CompileError::TypeMismatch),
        },
        AstExpression::Float(bits) => match expected {
            Some(ValueType::F32) => code.push(Instruction::LoadF32 {
                dst: destination,
                bits: (f64::from_bits(*bits) as f32).to_bits(),
            }),
            Some(ValueType::F64) | None => code.push(Instruction::LoadF64 {
                dst: destination,
                bits: *bits,
            }),
            Some(_) => return Err(CompileError::TypeMismatch),
        },
        AstExpression::Rune(value) => {
            if char::from_u32(*value).is_none() {
                return Err(CompileError::TypeMismatch);
            }
            code.push(Instruction::LoadRune {
                dst: destination,
                value: *value,
            });
        }
        AstExpression::String(value) => code.push(Instruction::LoadString {
            dst: destination,
            string: *context
                .string_indices
                .get(value)
                .expect("all string literals are collected before emission"),
        }),
        AstExpression::Bool(value) => code.push(Instruction::LoadBool {
            dst: destination,
            value: *value,
        }),
        AstExpression::Name(name) => {
            if let Some((source, _)) = locals.get(name) {
                code.push(Instruction::Move {
                    dst: destination,
                    source: *source,
                });
            } else if let Some(ValueType::Named(type_id)) = expected
                && let Some(variant) = context.enum_variants.get(&(type_id, name.clone()))
                && variant.payload_type.is_none()
            {
                code.push(Instruction::EnumNew {
                    type_id,
                    variant: variant.stable_id,
                    payload: None,
                    dst: destination,
                });
            } else {
                return Err(CompileError::UnknownName(name.clone()));
            }
        }
        AstExpression::StructLiteral { type_name, fields } => {
            let type_id = StableId::from_name(type_name);
            let struct_type = context
                .struct_types
                .iter()
                .find(|struct_type| struct_type.type_id == type_id)
                .ok_or_else(|| CompileError::UnknownType(type_name.clone()))?;
            let fields_base = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            for (index, metadata) in struct_type.fields.iter().enumerate() {
                let field = fields
                    .iter()
                    .find(|field| {
                        context
                            .struct_fields
                            .get(&(type_id, field.name.clone()))
                            .is_some_and(|candidate| candidate.stable_id == metadata.stable_id)
                    })
                    .ok_or(CompileError::TypeMismatch)?;
                let register = fields_base
                    .checked_add(u16::try_from(index).map_err(|_| CompileError::TooManyRegisters)?)
                    .ok_or(CompileError::TooManyRegisters)?;
                emit_expression(
                    &field.value,
                    register,
                    Some(metadata.ty),
                    locals,
                    context,
                    code,
                )?;
            }
            code.push(Instruction::StructNew {
                type_id,
                fields_base,
                fields_count: u16::try_from(struct_type.fields.len())
                    .map_err(|_| CompileError::TooManyRegisters)?,
                dst: destination,
            });
        }
        AstExpression::ClassNew { type_name, fields } => {
            let type_id = StableId::from_name(type_name);
            let class_type = context
                .class_types
                .iter()
                .find(|class_type| class_type.type_id == type_id)
                .ok_or_else(|| CompileError::UnknownType(type_name.clone()))?;
            let fields_base = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            for (index, metadata) in class_type.fields.iter().enumerate() {
                let field = fields
                    .iter()
                    .find(|field| {
                        context
                            .class_fields
                            .get(&(type_id, field.name.clone()))
                            .is_some_and(|candidate| candidate.stable_id == metadata.stable_id)
                    })
                    .ok_or(CompileError::TypeMismatch)?;
                let register = fields_base
                    .checked_add(u16::try_from(index).map_err(|_| CompileError::TooManyRegisters)?)
                    .ok_or(CompileError::TooManyRegisters)?;
                emit_expression(
                    &field.value,
                    register,
                    Some(metadata.ty),
                    locals,
                    context,
                    code,
                )?;
            }
            code.push(Instruction::ClassNew {
                type_id,
                fields_base,
                fields_count: u16::try_from(class_type.fields.len())
                    .map_err(|_| CompileError::TooManyRegisters)?,
                dst: destination,
            });
        }
        AstExpression::ArrayNew { element_type } => {
            code.push(Instruction::ArrayNew {
                type_id: nexa_bytecode::array_type(lower_type(element_type)),
                dst: destination,
            });
        }
        AstExpression::MapNew {
            key_type,
            value_type,
        } => {
            code.push(Instruction::MapNew {
                type_id: nexa_bytecode::map_type(lower_type(key_type), lower_type(value_type)),
                dst: destination,
            });
        }
        AstExpression::FieldGet { value, field } => {
            let ValueType::Named(type_id) = emitted_expression_type(value, None, locals, context)?
            else {
                return Err(CompileError::TypeMismatch);
            };
            emit_expression(
                value,
                destination,
                Some(ValueType::Named(type_id)),
                locals,
                context,
                code,
            )?;
            if let Some(field) = context.struct_fields.get(&(type_id, field.clone())) {
                code.push(Instruction::StructGet {
                    source: destination,
                    field: field.stable_id,
                    dst: destination,
                });
            } else if let Some(field) = context.class_fields.get(&(type_id, field.clone())) {
                code.push(Instruction::ClassGet {
                    source: destination,
                    field: field.stable_id,
                    dst: destination,
                });
            } else {
                return Err(CompileError::TypeMismatch);
            }
        }
        AstExpression::StructWith { value, updates } => {
            let ValueType::Named(type_id) = emitted_expression_type(value, None, locals, context)?
            else {
                return Err(CompileError::TypeMismatch);
            };
            emit_expression(
                value,
                destination,
                Some(ValueType::Named(type_id)),
                locals,
                context,
                code,
            )?;
            let replacement = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            for update in updates {
                let field = context
                    .struct_fields
                    .get(&(type_id, update.name.clone()))
                    .ok_or(CompileError::TypeMismatch)?;
                emit_expression(
                    &update.value,
                    replacement,
                    Some(field.ty),
                    locals,
                    context,
                    code,
                )?;
                code.push(Instruction::StructWith {
                    source: destination,
                    field: field.stable_id,
                    value: replacement,
                    dst: destination,
                });
            }
        }
        AstExpression::Binary { op, lhs, rhs } => {
            if op.kind == BinaryOp::Equal {
                let operand_type = emitted_expression_type(lhs, None, locals, context)?;
                let lhs_register = destination;
                let rhs_register = destination
                    .checked_add(1)
                    .ok_or(CompileError::TooManyRegisters)?;
                emit_expression(lhs, lhs_register, Some(operand_type), locals, context, code)?;
                emit_expression(rhs, rhs_register, Some(operand_type), locals, context, code)?;
                code.push(match operand_type {
                    ValueType::String => Instruction::StringEqual {
                        dst: destination,
                        lhs: lhs_register,
                        rhs: rhs_register,
                    },
                    ValueType::Named(type_id)
                        if context
                            .struct_types
                            .iter()
                            .any(|struct_type| struct_type.type_id == type_id) =>
                    {
                        Instruction::StructEqual {
                            dst: destination,
                            lhs: lhs_register,
                            rhs: rhs_register,
                        }
                    }
                    ValueType::Named(type_id)
                        if context
                            .class_types
                            .iter()
                            .any(|class_type| class_type.type_id == type_id) =>
                    {
                        Instruction::ClassEqual {
                            dst: destination,
                            lhs: lhs_register,
                            rhs: rhs_register,
                        }
                    }
                    _ => return Err(CompileError::TypeMismatch),
                });
                code.replace_span(previous_span);
                return Ok(());
            }
            let numeric_type =
                expected.unwrap_or(emitted_expression_type(lhs, None, locals, context)?);
            if op.kind == BinaryOp::Add && numeric_type == ValueType::String {
                let lhs_register = destination;
                let rhs_register = destination
                    .checked_add(1)
                    .ok_or(CompileError::TooManyRegisters)?;
                emit_expression(
                    lhs,
                    lhs_register,
                    Some(ValueType::String),
                    locals,
                    context,
                    code,
                )?;
                emit_expression(
                    rhs,
                    rhs_register,
                    Some(ValueType::String),
                    locals,
                    context,
                    code,
                )?;
                code.push(Instruction::StringConcat {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                });
                code.replace_span(previous_span);
                return Ok(());
            }
            if !matches!(
                numeric_type,
                ValueType::I32 | ValueType::I64 | ValueType::F32 | ValueType::F64
            ) {
                return Err(CompileError::TypeMismatch);
            }
            let lhs_register = destination;
            let rhs_register = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(lhs, lhs_register, Some(numeric_type), locals, context, code)?;
            emit_expression(rhs, rhs_register, Some(numeric_type), locals, context, code)?;
            code.push(match (numeric_type, op.kind) {
                (ValueType::I32, BinaryOp::Add) => Instruction::Add {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::I32, BinaryOp::Subtract) => Instruction::Sub {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::I32, BinaryOp::Multiply) => Instruction::Mul {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::I32, BinaryOp::Divide) => Instruction::Div {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::I64, BinaryOp::Add) => Instruction::AddI64 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::I64, BinaryOp::Subtract) => Instruction::SubI64 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::I64, BinaryOp::Multiply) => Instruction::MulI64 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::I64, BinaryOp::Divide) => Instruction::DivI64 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::F32, BinaryOp::Add) => Instruction::AddF32 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::F32, BinaryOp::Subtract) => Instruction::SubF32 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::F32, BinaryOp::Multiply) => Instruction::MulF32 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::F32, BinaryOp::Divide) => Instruction::DivF32 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::F64, BinaryOp::Add) => Instruction::AddF64 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::F64, BinaryOp::Subtract) => Instruction::SubF64 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::F64, BinaryOp::Multiply) => Instruction::MulF64 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                (ValueType::F64, BinaryOp::Divide) => Instruction::DivF64 {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                _ => return Err(CompileError::TypeMismatch),
            });
        }
        AstExpression::Call {
            function,
            arguments,
        } => {
            if let Some(ValueType::Named(type_id)) = expected
                && let Some(variant) = context.enum_variants.get(&(type_id, function.clone()))
            {
                let [payload] = arguments.as_slice() else {
                    return Err(CompileError::TypeMismatch);
                };
                let payload_type = variant.payload_type.ok_or(CompileError::TypeMismatch)?;
                let payload_register = destination
                    .checked_add(1)
                    .ok_or(CompileError::TooManyRegisters)?;
                emit_expression(
                    payload,
                    payload_register,
                    Some(payload_type),
                    locals,
                    context,
                    code,
                )?;
                code.push(Instruction::EnumNew {
                    type_id,
                    variant: variant.stable_id,
                    payload: Some(payload_register),
                    dst: destination,
                });
                code.replace_span(previous_span);
                return Ok(());
            }
            let args_base = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            let signature = context
                .host_functions
                .get(function)
                .map(|function| &function.signature)
                .or_else(|| {
                    context
                        .functions
                        .get(function)
                        .and_then(|index| context_function_signature(context, *index))
                })
                .ok_or_else(|| CompileError::UnknownName(function.clone()))?;
            for (index, argument) in arguments.iter().enumerate() {
                emit_expression(
                    argument,
                    args_base
                        .checked_add(
                            u16::try_from(index).map_err(|_| CompileError::TooManyRegisters)?,
                        )
                        .ok_or(CompileError::TooManyRegisters)?,
                    Some(signature.parameters[index]),
                    locals,
                    context,
                    code,
                )?;
            }
            let args_count =
                u16::try_from(arguments.len()).map_err(|_| CompileError::TooManyRegisters)?;
            if let Some(host) = context.host_functions.get(function) {
                code.push(Instruction::HostCall {
                    import: host.import,
                    args_base,
                    args_count,
                    dst: destination,
                });
            } else {
                code.push(Instruction::Call {
                    function: *context
                        .functions
                        .get(function)
                        .ok_or_else(|| CompileError::UnknownName(function.clone()))?,
                    args_base,
                    args_count,
                    dst: destination,
                });
            }
        }
        AstExpression::Await(expression) => {
            emit_expression(expression, destination, expected, locals, context, code)?;
            code.map_next(expression_span);
        }
        AstExpression::Constructor {
            type_name,
            variant,
            payload,
        } => {
            let type_id = if let Some(type_name) = type_name {
                let type_id = StableId::from_name(type_name);
                if expected.is_some_and(|expected| expected != ValueType::Named(type_id)) {
                    return Err(CompileError::TypeMismatch);
                }
                type_id
            } else {
                let ValueType::Named(type_id) = expected.ok_or(CompileError::CannotInferType)?
                else {
                    return Err(CompileError::TypeMismatch);
                };
                type_id
            };
            let metadata = context
                .enum_variants
                .get(&(type_id, variant.clone()))
                .ok_or(CompileError::TypeMismatch)?;
            let payload_register = if let Some(payload) = payload {
                let register = destination
                    .checked_add(1)
                    .ok_or(CompileError::TooManyRegisters)?;
                emit_expression(
                    payload,
                    register,
                    metadata.payload_type,
                    locals,
                    context,
                    code,
                )?;
                Some(register)
            } else {
                None
            };
            code.push(Instruction::EnumNew {
                type_id,
                variant: metadata.stable_id,
                payload: payload_register,
                dst: destination,
            });
        }
        AstExpression::Match { value, arms } => {
            emit_match_expression(value, arms, destination, expected, locals, context, code)?;
        }
        AstExpression::Try(expression) => {
            emit_try_expression(expression, destination, locals, context, code)?;
        }
        AstExpression::Migration(intrinsic) => {
            emit_migration_intrinsic(intrinsic, destination, locals, context, code)?;
        }
        AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
    }
    code.replace_span(previous_span);
    Ok(())
}

fn emit_string_method(
    function: &str,
    arguments: &[AstExpression],
    destination: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    let (receiver, method) =
        string_method(function).ok_or_else(|| CompileError::UnknownName(function.into()))?;
    let (source, ty) = *locals
        .get(receiver)
        .ok_or_else(|| CompileError::UnknownName(receiver.into()))?;
    if ty != ValueType::String {
        return Err(CompileError::TypeMismatch);
    }
    match method {
        StringMethod::Len => code.push(Instruction::StringLen {
            dst: destination,
            source,
        }),
        StringMethod::ByteLen => code.push(Instruction::StringByteLen {
            dst: destination,
            source,
        }),
        StringMethod::Hash => code.push(Instruction::StringHash {
            dst: destination,
            source,
        }),
        StringMethod::Equal | StringMethod::Concat => {
            let rhs = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                &arguments[0],
                rhs,
                Some(ValueType::String),
                locals,
                context,
                code,
            )?;
            code.push(if method == StringMethod::Equal {
                Instruction::StringEqual {
                    dst: destination,
                    lhs: source,
                    rhs,
                }
            } else {
                Instruction::StringConcat {
                    dst: destination,
                    lhs: source,
                    rhs,
                }
            });
        }
        StringMethod::RuneAt => {
            let index = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                &arguments[0],
                index,
                Some(ValueType::I32),
                locals,
                context,
                code,
            )?;
            code.push(Instruction::StringRuneAt {
                dst: destination,
                source,
                index,
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn emit_buffer_method(
    function: &str,
    arguments: &[AstExpression],
    destination: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    let (receiver, method) =
        buffer_method(function).ok_or_else(|| CompileError::UnknownName(function.into()))?;
    let (buffer, receiver_type) = *locals
        .get(receiver)
        .ok_or_else(|| CompileError::UnknownName(receiver.into()))?;
    let ValueType::Named(buffer_type) = receiver_type else {
        return Err(CompileError::TypeMismatch);
    };
    let element = context
        .buffer_types
        .iter()
        .find(|candidate| candidate.type_id == buffer_type)
        .map(|candidate| candidate.element)
        .ok_or(CompileError::TypeMismatch)?;
    let temporary = destination
        .checked_add(1)
        .ok_or(CompileError::TooManyRegisters)?;
    match method {
        BufferMethod::Len => code.push(Instruction::BufferLen {
            source: buffer,
            dst: destination,
        }),
        BufferMethod::Get => {
            emit_expression(
                &arguments[0],
                temporary,
                Some(ValueType::I32),
                locals,
                context,
                code,
            )?;
            code.push(Instruction::BufferGet {
                source: buffer,
                index: temporary,
                dst: destination,
            });
        }
        BufferMethod::Set => {
            let value = temporary
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                &arguments[0],
                temporary,
                Some(ValueType::I32),
                locals,
                context,
                code,
            )?;
            emit_expression(&arguments[1], value, Some(element), locals, context, code)?;
            code.push(Instruction::BufferSet {
                source: buffer,
                index: temporary,
                value,
            });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
        BufferMethod::Slice => {
            let length = temporary
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                &arguments[0],
                temporary,
                Some(ValueType::I32),
                locals,
                context,
                code,
            )?;
            emit_expression(
                &arguments[1],
                length,
                Some(ValueType::I32),
                locals,
                context,
                code,
            )?;
            code.push(Instruction::BufferSlice {
                source: buffer,
                start: temporary,
                length,
                dst: destination,
            });
        }
        BufferMethod::Copy => {
            let source_start = temporary
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            let destination_start = temporary
                .checked_add(2)
                .ok_or(CompileError::TooManyRegisters)?;
            let length = temporary
                .checked_add(3)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                &arguments[0],
                temporary,
                Some(receiver_type),
                locals,
                context,
                code,
            )?;
            for (argument, register) in
                arguments[1..]
                    .iter()
                    .zip([source_start, destination_start, length])
            {
                emit_expression(
                    argument,
                    register,
                    Some(ValueType::I32),
                    locals,
                    context,
                    code,
                )?;
            }
            code.push(Instruction::BufferCopy {
                destination: buffer,
                source: temporary,
                source_start,
                destination_start,
                length,
            });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn emit_map_method(
    function: &str,
    arguments: &[AstExpression],
    destination: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    let (receiver, method) =
        map_method(function).ok_or_else(|| CompileError::UnknownName(function.into()))?;
    let (source, receiver_type) = *locals
        .get(receiver)
        .ok_or_else(|| CompileError::UnknownName(receiver.into()))?;
    let ValueType::Named(map_type) = receiver_type else {
        return Err(CompileError::TypeMismatch);
    };
    let map_type = context
        .map_types
        .iter()
        .find(|candidate| candidate.type_id == map_type)
        .ok_or(CompileError::TypeMismatch)?;
    let temporary = destination
        .checked_add(1)
        .ok_or(CompileError::TooManyRegisters)?;
    match method {
        MapMethod::Len => code.push(Instruction::MapLen {
            source,
            dst: destination,
        }),
        MapMethod::Get | MapMethod::Remove => {
            emit_expression(
                &arguments[0],
                temporary,
                Some(map_type.key),
                locals,
                context,
                code,
            )?;
            let result_type = nexa_bytecode::option_type(map_type.value).type_id;
            code.push(if method == MapMethod::Get {
                Instruction::MapGet {
                    source,
                    key: temporary,
                    result_type,
                    dst: destination,
                }
            } else {
                Instruction::MapRemove {
                    source,
                    key: temporary,
                    result_type,
                    dst: destination,
                }
            });
        }
        MapMethod::Set => {
            let value = temporary
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                &arguments[0],
                temporary,
                Some(map_type.key),
                locals,
                context,
                code,
            )?;
            emit_expression(
                &arguments[1],
                value,
                Some(map_type.value),
                locals,
                context,
                code,
            )?;
            code.push(Instruction::MapSet {
                source,
                key: temporary,
                value,
            });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
        MapMethod::Contains => {
            emit_expression(
                &arguments[0],
                temporary,
                Some(map_type.key),
                locals,
                context,
                code,
            )?;
            code.push(Instruction::MapContains {
                source,
                key: temporary,
                dst: destination,
            });
        }
        MapMethod::Clear => {
            code.push(Instruction::MapClear { source });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn emit_array_method(
    function: &str,
    arguments: &[AstExpression],
    destination: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    let (receiver, method) =
        array_method(function).ok_or_else(|| CompileError::UnknownName(function.into()))?;
    let (source, receiver_type) = *locals
        .get(receiver)
        .ok_or_else(|| CompileError::UnknownName(receiver.into()))?;
    let ValueType::Named(array_type) = receiver_type else {
        return Err(CompileError::TypeMismatch);
    };
    let element = context
        .array_types
        .iter()
        .find(|candidate| candidate.type_id == array_type)
        .map(|candidate| candidate.element)
        .ok_or(CompileError::TypeMismatch)?;
    let temporary = destination
        .checked_add(1)
        .ok_or(CompileError::TooManyRegisters)?;
    match method {
        ArrayMethod::Len => code.push(Instruction::ArrayLen {
            source,
            dst: destination,
        }),
        ArrayMethod::Get | ArrayMethod::Remove => {
            emit_expression(
                &arguments[0],
                temporary,
                Some(ValueType::I32),
                locals,
                context,
                code,
            )?;
            code.push(if method == ArrayMethod::Get {
                Instruction::ArrayGet {
                    source,
                    index: temporary,
                    dst: destination,
                }
            } else {
                Instruction::ArrayRemove {
                    source,
                    index: temporary,
                    dst: destination,
                }
            });
        }
        ArrayMethod::Set | ArrayMethod::Insert => {
            let value = temporary
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                &arguments[0],
                temporary,
                Some(ValueType::I32),
                locals,
                context,
                code,
            )?;
            emit_expression(&arguments[1], value, Some(element), locals, context, code)?;
            code.push(if method == ArrayMethod::Set {
                Instruction::ArraySet {
                    source,
                    index: temporary,
                    value,
                }
            } else {
                Instruction::ArrayInsert {
                    source,
                    index: temporary,
                    value,
                }
            });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
        ArrayMethod::Push => {
            emit_expression(
                &arguments[0],
                temporary,
                Some(element),
                locals,
                context,
                code,
            )?;
            code.push(Instruction::ArrayPush {
                source,
                value: temporary,
            });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
        ArrayMethod::Pop => code.push(Instruction::ArrayPop {
            source,
            dst: destination,
        }),
        ArrayMethod::Clear => {
            code.push(Instruction::ArrayClear { source });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
    }
    Ok(())
}

fn emit_state_handle_method(
    function: &str,
    arguments: &[AstExpression],
    destination: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    let (receiver, method) =
        state_handle_method(function).ok_or_else(|| CompileError::UnknownName(function.into()))?;
    let (handle, receiver_type) = *locals
        .get(receiver)
        .ok_or_else(|| CompileError::UnknownName(receiver.into()))?;
    let ValueType::Named(handle_type) = receiver_type else {
        return Err(CompileError::TypeMismatch);
    };
    let target = *context
        .state_handle_targets
        .get(&handle_type)
        .ok_or(CompileError::TypeMismatch)?;
    match method {
        StateHandleMethod::Resolve => code.push(Instruction::StateHandleResolve {
            handle,
            target,
            result_type: nexa_bytecode::result_type(
                target,
                ValueType::Named(nexa_bytecode::state_handle_error_type().type_id),
            )
            .type_id,
            dst: destination,
        }),
        StateHandleMethod::IsAlive => code.push(Instruction::StateHandleIsAlive {
            handle,
            target,
            dst: destination,
        }),
        StateHandleMethod::StableId => code.push(Instruction::StateHandleStableId {
            handle,
            target,
            dst: destination,
        }),
        StateHandleMethod::Generation => code.push(Instruction::StateHandleGeneration {
            handle,
            target,
            dst: destination,
        }),
        StateHandleMethod::Equality => {
            let rhs = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                &arguments[0],
                rhs,
                Some(receiver_type),
                locals,
                context,
                code,
            )?;
            code.push(Instruction::StateHandleEqual {
                lhs: handle,
                rhs,
                target,
                dst: destination,
            });
        }
        StateHandleMethod::Hash => code.push(Instruction::StateHandleHash {
            handle,
            target,
            dst: destination,
        }),
    }
    Ok(())
}

fn context_function_signature<'a>(
    context: &'a EmitContext<'a>,
    index: u32,
) -> Option<&'a Signature> {
    context
        .script_functions
        .get(index as usize)
        .map(|function| &function.signature)
}

#[allow(clippy::too_many_lines)]
fn emitted_expression_type(
    expression: &AstExpression,
    expected: Option<ValueType>,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
) -> Result<ValueType, CompileError> {
    match expression.kind() {
        AstExpression::Integer(value) => match expected {
            Some(ValueType::I32) | None if i32::try_from(*value).is_ok() => Ok(ValueType::I32),
            Some(ValueType::I64) | None => Ok(ValueType::I64),
            Some(_) => Err(CompileError::TypeMismatch),
        },
        AstExpression::Float(_) => match expected {
            Some(ValueType::F32) => Ok(ValueType::F32),
            Some(ValueType::F64) | None => Ok(ValueType::F64),
            Some(_) => Err(CompileError::TypeMismatch),
        },
        AstExpression::Rune(value) => char::from_u32(*value)
            .map(|_| ValueType::Rune)
            .ok_or(CompileError::TypeMismatch),
        AstExpression::String(_) => Ok(ValueType::String),
        AstExpression::Binary { op, lhs, .. } if op.kind == BinaryOp::Equal => Ok(ValueType::Bool),
        AstExpression::Binary { op, lhs, .. } => {
            let ty = if let Some(expected) = expected {
                expected
            } else {
                emitted_expression_type(lhs, None, locals, context)?
            };
            if op.kind == BinaryOp::Add && ty == ValueType::String {
                return Ok(ValueType::String);
            }
            if matches!(
                ty,
                ValueType::I32 | ValueType::I64 | ValueType::F32 | ValueType::F64
            ) {
                Ok(ty)
            } else {
                Err(CompileError::TypeMismatch)
            }
        }
        AstExpression::Bool(_) => Ok(ValueType::Bool),
        AstExpression::Name(name) => locals.get(name).map_or_else(
            || {
                let Some(ValueType::Named(type_id)) = expected else {
                    return Err(CompileError::UnknownName(name.clone()));
                };
                context
                    .enum_variants
                    .get(&(type_id, name.clone()))
                    .filter(|variant| variant.payload_type.is_none())
                    .map(|_| ValueType::Named(type_id))
                    .ok_or_else(|| CompileError::UnknownName(name.clone()))
            },
            |(_, ty)| Ok(*ty),
        ),
        AstExpression::StructLiteral { type_name, .. } => {
            let type_id = StableId::from_name(type_name);
            context
                .struct_types
                .iter()
                .any(|struct_type| struct_type.type_id == type_id)
                .then_some(ValueType::Named(type_id))
                .ok_or_else(|| CompileError::UnknownType(type_name.clone()))
        }
        AstExpression::ClassNew { type_name, .. } => {
            let type_id = StableId::from_name(type_name);
            context
                .class_types
                .iter()
                .any(|class_type| class_type.type_id == type_id)
                .then_some(ValueType::Named(type_id))
                .ok_or_else(|| CompileError::UnknownType(type_name.clone()))
        }
        AstExpression::ArrayNew { element_type } => Ok(ValueType::Named(
            nexa_bytecode::array_type(lower_type(element_type)),
        )),
        AstExpression::MapNew {
            key_type,
            value_type,
        } => Ok(ValueType::Named(nexa_bytecode::map_type(
            lower_type(key_type),
            lower_type(value_type),
        ))),
        AstExpression::FieldGet { value, field } => {
            let ValueType::Named(type_id) = emitted_expression_type(value, None, locals, context)?
            else {
                return Err(CompileError::TypeMismatch);
            };
            context
                .struct_fields
                .get(&(type_id, field.clone()))
                .or_else(|| context.class_fields.get(&(type_id, field.clone())))
                .map(|field| field.ty)
                .ok_or(CompileError::TypeMismatch)
        }
        AstExpression::StructWith { value, .. } => {
            emitted_expression_type(value, None, locals, context)
        }
        AstExpression::Call { function, .. } => {
            if let Some(ValueType::Named(type_id)) = expected
                && context
                    .enum_variants
                    .get(&(type_id, function.clone()))
                    .is_some()
            {
                Ok(ValueType::Named(type_id))
            } else if let Some((receiver, method)) = string_method(function)
                && locals.get(receiver).map(|(_, ty)| *ty) == Some(ValueType::String)
            {
                Ok(match method {
                    StringMethod::Len | StringMethod::ByteLen => ValueType::I32,
                    StringMethod::Hash => ValueType::I64,
                    StringMethod::Equal => ValueType::Bool,
                    StringMethod::Concat => ValueType::String,
                    StringMethod::RuneAt => ValueType::Rune,
                })
            } else if let Some((receiver, method)) = state_handle_method(function) {
                let ValueType::Named(handle_type) = locals
                    .get(receiver)
                    .map(|(_, ty)| *ty)
                    .ok_or_else(|| CompileError::UnknownName(receiver.into()))?
                else {
                    return Err(CompileError::TypeMismatch);
                };
                let target = *context
                    .state_handle_targets
                    .get(&handle_type)
                    .ok_or(CompileError::TypeMismatch)?;
                Ok(match method {
                    StateHandleMethod::Resolve => ValueType::Named(
                        nexa_bytecode::result_type(
                            target,
                            ValueType::Named(nexa_bytecode::state_handle_error_type().type_id),
                        )
                        .type_id,
                    ),
                    StateHandleMethod::IsAlive | StateHandleMethod::Equality => ValueType::Bool,
                    StateHandleMethod::StableId => nexa_bytecode::stable_id_type(),
                    StateHandleMethod::Generation | StateHandleMethod::Hash => ValueType::I32,
                })
            } else if let Some((receiver, method)) = buffer_method(function)
                && locals.get(receiver).is_some_and(|(_, ty)| {
                    matches!(ty, ValueType::Named(type_id) if context
                        .buffer_types
                        .iter()
                        .any(|buffer| buffer.type_id == *type_id))
                })
            {
                let receiver_type = locals
                    .get(receiver)
                    .map(|(_, ty)| *ty)
                    .ok_or_else(|| CompileError::UnknownName(receiver.into()))?;
                let ValueType::Named(buffer_type) = receiver_type else {
                    return Err(CompileError::TypeMismatch);
                };
                let element = context
                    .buffer_types
                    .iter()
                    .find(|buffer| buffer.type_id == buffer_type)
                    .map(|buffer| buffer.element)
                    .ok_or(CompileError::TypeMismatch)?;
                Ok(match method {
                    BufferMethod::Len => ValueType::I32,
                    BufferMethod::Get => element,
                    BufferMethod::Slice => receiver_type,
                    BufferMethod::Set | BufferMethod::Copy => ValueType::Bool,
                })
            } else if let Some((receiver, method)) = map_method(function)
                && locals.get(receiver).is_some_and(|(_, ty)| {
                    matches!(ty, ValueType::Named(type_id) if context
                        .map_types
                        .iter()
                        .any(|map_type| map_type.type_id == *type_id))
                })
            {
                let ValueType::Named(map_type) = locals
                    .get(receiver)
                    .map(|(_, ty)| *ty)
                    .ok_or_else(|| CompileError::UnknownName(receiver.into()))?
                else {
                    return Err(CompileError::TypeMismatch);
                };
                let map_type = context
                    .map_types
                    .iter()
                    .find(|candidate| candidate.type_id == map_type)
                    .ok_or(CompileError::TypeMismatch)?;
                Ok(match method {
                    MapMethod::Len => ValueType::I32,
                    MapMethod::Get | MapMethod::Remove => {
                        ValueType::Named(nexa_bytecode::option_type(map_type.value).type_id)
                    }
                    MapMethod::Set | MapMethod::Contains | MapMethod::Clear => ValueType::Bool,
                })
            } else if let Some((receiver, method)) = array_method(function) {
                let ValueType::Named(array_type) = locals
                    .get(receiver)
                    .map(|(_, ty)| *ty)
                    .ok_or_else(|| CompileError::UnknownName(receiver.into()))?
                else {
                    return Err(CompileError::TypeMismatch);
                };
                let element = context
                    .array_types
                    .iter()
                    .find(|candidate| candidate.type_id == array_type)
                    .map(|candidate| candidate.element)
                    .ok_or(CompileError::TypeMismatch)?;
                Ok(match method {
                    ArrayMethod::Len => ValueType::I32,
                    ArrayMethod::Get | ArrayMethod::Pop | ArrayMethod::Remove => element,
                    ArrayMethod::Set
                    | ArrayMethod::Push
                    | ArrayMethod::Insert
                    | ArrayMethod::Clear => ValueType::Bool,
                })
            } else {
                context
                    .host_functions
                    .get(function)
                    .map(|function| &function.signature)
                    .or_else(|| {
                        context
                            .functions
                            .get(function)
                            .and_then(|index| context_function_signature(context, *index))
                    })
                    .and_then(|signature| signature.result)
                    .ok_or_else(|| CompileError::UnknownName(function.clone()))
            }
        }
        AstExpression::Await(expression) => {
            emitted_expression_type(expression, expected, locals, context)
        }
        AstExpression::Constructor { type_name, .. } => type_name.as_ref().map_or_else(
            || expected.ok_or(CompileError::CannotInferType),
            |type_name| Ok(ValueType::Named(StableId::from_name(type_name))),
        ),
        AstExpression::Match { .. } => expected.ok_or(CompileError::CannotInferType),
        AstExpression::Try(expression) => {
            let ValueType::Named(type_id) =
                emitted_expression_type(expression, None, locals, context)?
            else {
                return Err(CompileError::TypeMismatch);
            };
            context
                .enum_variants
                .get(&(type_id, "Ok".to_owned()))
                .and_then(|variant| variant.payload_type)
                .ok_or(CompileError::TypeMismatch)
        }
        AstExpression::Migration(intrinsic) => Ok(match intrinsic.kind() {
            MigrationIntrinsic::OldGet { ty, .. }
            | MigrationIntrinsic::OldFieldGet { ty, .. }
            | MigrationIntrinsic::NewCreate { ty, .. } => lower_type(ty),
            MigrationIntrinsic::NewSet { .. }
            | MigrationIntrinsic::Preserve { .. }
            | MigrationIntrinsic::Replace { .. }
            | MigrationIntrinsic::Delete { .. }
            | MigrationIntrinsic::Finish => ValueType::Bool,
            MigrationIntrinsic::Spanned { .. } => unreachable!("kind strips spans"),
        }),
        AstExpression::Spanned { .. } => unreachable!("kind strips spans"),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_match_expression(
    value: &AstExpression,
    arms: &[MatchArm],
    destination: u16,
    expected: Option<ValueType>,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    let result_type = expected.ok_or(CompileError::CannotInferType)?;
    let ValueType::Named(type_id) = emitted_expression_type(value, None, locals, context)? else {
        return Err(CompileError::TypeMismatch);
    };
    emit_expression(
        value,
        destination,
        Some(ValueType::Named(type_id)),
        locals,
        context,
        code,
    )?;
    let tag_register = destination
        .checked_add(1)
        .ok_or(CompileError::TooManyRegisters)?;
    let expected_tag = destination
        .checked_add(2)
        .ok_or(CompileError::TooManyRegisters)?;
    let condition = destination
        .checked_add(3)
        .ok_or(CompileError::TooManyRegisters)?;
    code.push(Instruction::EnumTag {
        source: destination,
        dst: tag_register,
    });
    let mut success_jumps = Vec::with_capacity(arms.len());
    let mut pending_false = None;
    for arm in arms {
        let arm_start = u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
        if let Some(branch) = pending_false.take() {
            code[branch] = Instruction::JumpIfFalse {
                condition,
                target: arm_start,
            };
        }
        let variant = context
            .enum_variants
            .get(&(type_id, arm.variant.clone()))
            .ok_or(CompileError::TypeMismatch)?;
        code.push(Instruction::LoadI32 {
            dst: expected_tag,
            value: i32::from_ne_bytes(variant.tag.to_ne_bytes()),
        });
        code.push(Instruction::CompareEq {
            dst: condition,
            lhs: tag_register,
            rhs: expected_tag,
        });
        let branch = code.len();
        code.push(Instruction::JumpIfFalse {
            condition,
            target: 0,
        });
        pending_false = Some(branch);
        if let Some(binding) = &arm.binding {
            code.push(Instruction::EnumPayload {
                source: destination,
                variant: variant.stable_id,
                dst: locals[binding].0,
            });
        }
        emit_expression(
            &arm.value,
            destination,
            Some(result_type),
            locals,
            context,
            code,
        )?;
        success_jumps.push(code.len());
        code.push(Instruction::Jump { target: 0 });
    }
    let trap = u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
    if let Some(branch) = pending_false {
        code[branch] = Instruction::JumpIfFalse {
            condition,
            target: trap,
        };
    }
    code.push(Instruction::Trap);
    let end = u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
    code.push(Instruction::Safepoint);
    for jump in success_jumps {
        code[jump] = Instruction::Jump { target: end };
    }
    Ok(())
}

fn emit_try_expression(
    expression: &AstExpression,
    destination: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    let ValueType::Named(type_id) = emitted_expression_type(expression, None, locals, context)?
    else {
        return Err(CompileError::TypeMismatch);
    };
    let ok = context
        .enum_variants
        .get(&(type_id, "Ok".to_owned()))
        .ok_or(CompileError::TypeMismatch)?;
    let error = context
        .enum_variants
        .get(&(type_id, "Err".to_owned()))
        .ok_or(CompileError::TypeMismatch)?;
    let ValueType::Named(function_result) = context.function_result else {
        return Err(CompileError::TypeMismatch);
    };
    let result_error = context
        .enum_variants
        .get(&(function_result, "Err".to_owned()))
        .ok_or(CompileError::TypeMismatch)?;
    emit_expression(
        expression,
        destination,
        Some(ValueType::Named(type_id)),
        locals,
        context,
        code,
    )?;
    let tag = destination
        .checked_add(1)
        .ok_or(CompileError::TooManyRegisters)?;
    let expected_tag = destination
        .checked_add(2)
        .ok_or(CompileError::TooManyRegisters)?;
    let condition = destination
        .checked_add(3)
        .ok_or(CompileError::TooManyRegisters)?;
    code.push(Instruction::EnumTag {
        source: destination,
        dst: tag,
    });
    code.push(Instruction::LoadI32 {
        dst: expected_tag,
        value: i32::from_ne_bytes(ok.tag.to_ne_bytes()),
    });
    code.push(Instruction::CompareEq {
        dst: condition,
        lhs: tag,
        rhs: expected_tag,
    });
    let error_branch = code.len();
    code.push(Instruction::JumpIfFalse {
        condition,
        target: 0,
    });
    code.push(Instruction::EnumPayload {
        source: destination,
        variant: ok.stable_id,
        dst: destination,
    });
    let success = code.len();
    code.push(Instruction::Jump { target: 0 });
    let error_start = u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
    code[error_branch] = Instruction::JumpIfFalse {
        condition,
        target: error_start,
    };
    code.push(Instruction::EnumPayload {
        source: destination,
        variant: error.stable_id,
        dst: tag,
    });
    code.push(Instruction::EnumNew {
        type_id: function_result,
        variant: result_error.stable_id,
        payload: Some(tag),
        dst: destination,
    });
    code.push(Instruction::Return {
        source: destination,
    });
    let end = u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
    code.push(Instruction::Safepoint);
    code[success] = Instruction::Jump { target: end };
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn emit_migration_intrinsic(
    intrinsic: &MigrationIntrinsic,
    destination: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut TrackedCode,
) -> Result<(), CompileError> {
    match intrinsic.kind() {
        MigrationIntrinsic::OldGet { stable_id, ty } => {
            code.push(Instruction::StateOldGet {
                stable_id: StableId::from_name(stable_id),
                ty: lower_type(ty),
                dst: destination,
            });
        }
        MigrationIntrinsic::OldFieldGet {
            object,
            owner,
            field,
            ty,
        } => {
            let object_register = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                object,
                object_register,
                Some(ValueType::Named(StableId::from_name(owner))),
                locals,
                context,
                code,
            )?;
            code.push(Instruction::StateOldFieldGet {
                object: object_register,
                field_id: StableId::from_parts(&[owner, "::", field]),
                ty: lower_type(ty),
                dst: destination,
            });
        }
        MigrationIntrinsic::NewCreate { stable_id, ty } => {
            let ValueType::Named(type_id) = lower_type(ty) else {
                return Err(CompileError::TypeMismatch);
            };
            code.push(Instruction::StateNewCreate {
                stable_id: StableId::from_name(stable_id),
                type_id,
                dst: destination,
            });
        }
        MigrationIntrinsic::NewSet {
            object,
            owner,
            field,
            value,
        } => {
            let owner_type = StableId::from_name(owner);
            let field_id = StableId::from_parts(&[owner, "::", field]);
            let field_type = context
                .state_schema
                .types
                .iter()
                .find(|state_type| state_type.stable_id == owner_type)
                .and_then(|state_type| {
                    state_type
                        .fields
                        .iter()
                        .find(|candidate| candidate.stable_id == field_id)
                })
                .map(|field| field.ty)
                .ok_or(CompileError::TypeMismatch)?;
            emit_expression(
                object,
                destination,
                Some(ValueType::Named(owner_type)),
                locals,
                context,
                code,
            )?;
            let value_register = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                value,
                value_register,
                Some(field_type),
                locals,
                context,
                code,
            )?;
            code.push(Instruction::StateNewSet {
                object: destination,
                field_id,
                source: value_register,
            });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
        MigrationIntrinsic::Preserve { stable_id } => {
            code.push(Instruction::StatePreserve {
                stable_id: StableId::from_name(stable_id),
            });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
        MigrationIntrinsic::Replace { stable_id, target } => {
            emit_expression(target, destination, None, locals, context, code)?;
            code.push(Instruction::StateReplace {
                old_id: StableId::from_name(stable_id),
                target: destination,
            });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
        MigrationIntrinsic::Delete { stable_id } => {
            code.push(Instruction::StateDelete {
                stable_id: StableId::from_name(stable_id),
            });
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
        MigrationIntrinsic::Finish => {
            code.push(Instruction::StateFinish);
            code.push(Instruction::LoadBool {
                dst: destination,
                value: true,
            });
        }
        MigrationIntrinsic::Spanned { .. } => unreachable!("kind strips spans"),
    }
    Ok(())
}

fn collect_safepoints(code: &[Instruction]) -> Vec<u32> {
    let mut safepoints = code
        .iter()
        .enumerate()
        .filter_map(|(pc, instruction)| {
            let pc = u32::try_from(pc).ok()?;
            let explicit = matches!(
                instruction,
                Instruction::Safepoint
                    | Instruction::LoadString { .. }
                    | Instruction::StringConcat { .. }
                    | Instruction::EnumNew { .. }
                    | Instruction::StructNew { .. }
                    | Instruction::StructWith { .. }
                    | Instruction::ClassNew { .. }
                    | Instruction::ArrayNew { .. }
                    | Instruction::ArrayLen { .. }
                    | Instruction::ArrayGet { .. }
                    | Instruction::ArraySet { .. }
                    | Instruction::ArrayPush { .. }
                    | Instruction::ArrayPop { .. }
                    | Instruction::ArrayInsert { .. }
                    | Instruction::ArrayRemove { .. }
                    | Instruction::ArrayClear { .. }
                    | Instruction::MapNew { .. }
                    | Instruction::MapLen { .. }
                    | Instruction::MapGet { .. }
                    | Instruction::MapSet { .. }
                    | Instruction::MapRemove { .. }
                    | Instruction::MapContains { .. }
                    | Instruction::MapClear { .. }
                    | Instruction::BufferLen { .. }
                    | Instruction::BufferGet { .. }
                    | Instruction::BufferSet { .. }
                    | Instruction::BufferSlice { .. }
                    | Instruction::BufferCopy { .. }
                    | Instruction::Yield
                    | Instruction::Call { .. }
                    | Instruction::HostCall { .. }
                    | Instruction::StateHandleResolve { .. }
                    | Instruction::Return { .. }
                    | Instruction::ReturnVoid
                    | Instruction::Trap
                    | Instruction::CleanupReturn
            );
            let back_edge = matches!(instruction, Instruction::Jump { target } if *target <= pc)
                || matches!(
                    instruction,
                    Instruction::JumpIfFalse { target, .. } if *target <= pc
                );
            (pc == 0 || explicit || back_edge).then_some(pc)
        })
        .collect::<Vec<_>>();
    for (pc, instruction) in code.iter().enumerate() {
        if matches!(instruction, Instruction::HostCall { .. }) && pc + 1 < code.len() {
            safepoints.push(u32::try_from(pc + 1).expect("instruction index is bounded"));
        }
    }
    safepoints.sort_unstable();
    safepoints.dedup();
    safepoints
}

pub fn compile(source: &str) -> Result<VerifiedModule, CompileError> {
    compile_module(source, None)
}

pub fn compile_with_metadata(
    source: &str,
    host_hash: StableId,
    schema_hash: StableId,
) -> Result<VerifiedModule, CompileError> {
    compile_module(source, Some((host_hash, schema_hash)))
}

#[allow(clippy::too_many_lines)]
pub fn compile_with_interface(
    source: &str,
    interface: &Idl,
    schema_hash: StableId,
) -> Result<VerifiedModule, CompileError> {
    let tokens = lex(source)?;
    let mut ast = parse(&tokens)?;
    for structure in &interface.structs {
        if !ast
            .types
            .iter()
            .any(|declaration| declaration.name == structure.name)
        {
            ast.types.push(AstTypeDeclaration {
                name: structure.name.clone(),
                kind: AstTypeKind::Struct,
                version: 0,
                fields: structure
                    .fields
                    .iter()
                    .map(|field| AstField {
                        name: field.name.clone(),
                        ty: ast_type_from_idl(&field.ty),
                        span: SourceSpan::new(FileId(0), 0, 0),
                    })
                    .collect(),
                variants: Vec::new(),
                span: SourceSpan::new(FileId(0), 0, 0),
            });
        }
    }
    for enumeration in &interface.enums {
        if !ast
            .types
            .iter()
            .any(|declaration| declaration.name == enumeration.name)
        {
            ast.types.push(AstTypeDeclaration {
                name: enumeration.name.clone(),
                kind: AstTypeKind::Enum,
                version: 0,
                fields: Vec::new(),
                variants: enumeration
                    .variants
                    .iter()
                    .map(|variant| AstVariant {
                        name: variant.name.clone(),
                        payload: variant.payload.as_ref().map(ast_type_from_idl),
                        span: SourceSpan::new(FileId(0), 0, 0),
                    })
                    .collect(),
                span: SourceSpan::new(FileId(0), 0, 0),
            });
        }
    }
    let import = ast
        .imports
        .first()
        .and_then(|import| import.name.rsplit('.').next())
        .ok_or_else(|| CompileError::UnknownName("missing host import".into()))?;
    let mut host_functions = BTreeMap::new();
    for (index, function) in interface.functions.iter().enumerate() {
        let parameters = function
            .parameters
            .iter()
            .map(|parameter| lower_idl_type(&parameter.ty))
            .collect::<Vec<_>>();
        let (result, async_result) = if function.synchronous {
            (Some(lower_idl_type(&function.result)), None)
        } else {
            let (success, error) = match &function.result {
                TypeRef::HostRequest(Some(result)) => match result.as_ref() {
                    TypeRef::Result(success, error) => {
                        (lower_idl_type(success), lower_idl_type(error))
                    }
                    _ => return Err(CompileError::TypeMismatch),
                },
                _ => return Err(CompileError::TypeMismatch),
            };
            let result_type = nexa_bytecode::result_type(success, error).type_id;
            let policy_error = |ty: &TypeRef, variant: &str, fallback: u32| match ty {
                TypeRef::I32 => Some(fallback),
                TypeRef::Named(name) => interface
                    .enums
                    .iter()
                    .find(|enumeration| enumeration.name == *name)
                    .and_then(|enumeration| {
                        enumeration.variants.iter().position(|candidate| {
                            candidate.name == variant && candidate.payload.is_none()
                        })
                    })
                    .and_then(|tag| u32::try_from(tag).ok()),
                _ => None,
            };
            let TypeRef::HostRequest(Some(request_result)) = &function.result else {
                unreachable!("typed request shape checked above");
            };
            let TypeRef::Result(_, error_ref) = request_result.as_ref() else {
                unreachable!("typed request shape checked above");
            };
            let cancel_error = match function.cancel_policy {
                nexa_idl::CancelPolicy::ReturnError => Some(
                    policy_error(error_ref, "Cancelled", u32::MAX - 1)
                        .ok_or(CompileError::TypeMismatch)?,
                ),
                nexa_idl::CancelPolicy::CancelTask => None,
            };
            let abandon_error = match function.abandon_policy {
                nexa_idl::AbandonPolicy::ReturnError => Some(
                    policy_error(error_ref, "Abandoned", u32::MAX)
                        .ok_or(CompileError::TypeMismatch)?,
                ),
                nexa_idl::AbandonPolicy::Trap => None,
            };
            (
                Some(ValueType::Named(result_type)),
                Some(AsyncResultType {
                    result_type,
                    success,
                    error,
                    cancel_policy: match function.cancel_policy {
                        nexa_idl::CancelPolicy::ReturnError => CancelPolicy::ReturnError,
                        nexa_idl::CancelPolicy::CancelTask => CancelPolicy::CancelTask,
                    },
                    abandon_policy: match function.abandon_policy {
                        nexa_idl::AbandonPolicy::ReturnError => AbandonPolicy::ReturnError,
                        nexa_idl::AbandonPolicy::Trap => AbandonPolicy::Trap,
                    },
                    cancel_error,
                    abandon_error,
                }),
            )
        };
        let metadata = HostImport {
            stable_id: StableId::from_parts(&[&interface.interface, "::", &function.name]),
            parameters: parameters.clone(),
            result,
            mode: if function.synchronous {
                HostCallMode::Immediate
            } else {
                HostCallMode::Async
            },
            fuel_cost: 1,
            async_result,
        };
        host_functions.insert(
            format!("{import}.{}", function.name),
            HostFunction {
                import: u32::try_from(index).map_err(|_| CompileError::TooManyRegisters)?,
                signature: Signature { parameters, result },
                metadata,
            },
        );
    }
    let host_hash = nexa_idl::exact_hash(interface);
    let hir =
        resolve_and_typecheck_with_hosts(ast, host_functions, Some(host_hash), Some(schema_hash))?;
    let mut module = emit_bytecode(&hir)?;
    for export in &interface.exports {
        let (function, hir_function) = hir
            .functions
            .iter()
            .enumerate()
            .find(|(_, function)| function.name.eq_ignore_ascii_case(&export.name))
            .ok_or_else(|| CompileError::UnknownName(export.name.clone()))?;
        let signature = Signature {
            parameters: export
                .parameters
                .iter()
                .map(|parameter| lower_idl_type(&parameter.ty))
                .collect(),
            result: export.result.as_ref().map(lower_idl_type),
        };
        if hir_function.signature != signature {
            return Err(CompileError::TypeMismatch);
        }
        module.exports.push(ScriptExport {
            stable_id: StableId::from_parts(&[&interface.interface, "::export::", &export.name]),
            function: u32::try_from(function).map_err(|_| CompileError::TooManyRegisters)?,
            signature,
        });
    }
    verify(module, VerifierLimits::default())
        .map_err(|error| CompileError::Verify(error.to_string()))
}

fn lower_idl_type(ty: &TypeRef) -> ValueType {
    match ty {
        TypeRef::I32 => ValueType::I32,
        TypeRef::I64 => ValueType::I64,
        TypeRef::F32 => ValueType::F32,
        TypeRef::F64 => ValueType::F64,
        TypeRef::Bool => ValueType::Bool,
        TypeRef::Rune => ValueType::Rune,
        TypeRef::HostRequest(_) => ValueType::Named(StableId::from_name("HostRequest")),
        TypeRef::ResourceToken(_) => ValueType::Named(StableId::from_name("ResourceToken")),
        TypeRef::Snapshot(_) => ValueType::Named(StableId::from_name("Snapshot")),
        TypeRef::Option(inner) => {
            ValueType::Named(nexa_bytecode::option_type(lower_idl_type(inner)).type_id)
        }
        TypeRef::Result(success, error) => ValueType::Named(
            nexa_bytecode::result_type(lower_idl_type(success), lower_idl_type(error)).type_id,
        ),
        TypeRef::String => ValueType::String,
        TypeRef::Named(name) => ValueType::Named(StableId::from_name(name)),
    }
}

fn ast_type_from_idl(ty: &TypeRef) -> AstType {
    match ty {
        TypeRef::I32 => AstType::I32,
        TypeRef::I64 => AstType::I64,
        TypeRef::F32 => AstType::F32,
        TypeRef::F64 => AstType::F64,
        TypeRef::Bool => AstType::Bool,
        TypeRef::Rune => AstType::Rune,
        TypeRef::String => AstType::String,
        TypeRef::HostRequest(_) => AstType::Named("HostRequest".into()),
        TypeRef::ResourceToken(_) => AstType::Named("ResourceToken".into()),
        TypeRef::Snapshot(_) => AstType::Named("Snapshot".into()),
        TypeRef::Option(inner) => AstType::BuiltinGeneric {
            name: "Option".into(),
            arguments: vec![ast_type_from_idl(inner)],
        },
        TypeRef::Result(success, error) => AstType::BuiltinGeneric {
            name: "Result".into(),
            arguments: vec![ast_type_from_idl(success), ast_type_from_idl(error)],
        },
        TypeRef::Named(name) => AstType::Named(name.clone()),
    }
}

fn compile_module(
    source: &str,
    metadata: Option<(StableId, StableId)>,
) -> Result<VerifiedModule, CompileError> {
    let tokens = lex(source)?;
    let ast = parse(&tokens)?;
    let hir = resolve_and_typecheck(ast)?;
    let mut module = emit_bytecode(&hir)?;
    if let Some((host_hash, schema_hash)) = metadata {
        module.host_interface_hash = Some(host_hash);
        module.schema_hash = Some(schema_hash);
    }
    verify(module, VerifierLimits::default())
        .map_err(|error| CompileError::Verify(error.to_string()))
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{FunctionEffect, Instruction};
    use nexa_core::{FileId, StableId};
    use nexa_runtime::{CheckedInterpreter, GcRef, InterpreterOutcome, RuntimeValue, TrapKind};

    use super::{
        AstExpression, AstStatement, AstType, CompileError, compile, lex, parse_with_file,
        resolve_and_typecheck,
    };

    #[test]
    fn every_required_ast_and_hir_node_preserves_its_source_span() {
        let source = "module demo.core;
import host.api;
struct Pair { value: Option<Result<i32, bool>>; }
enum Choice { A, B }
task fn run(input: Option<i32>) -> Result<i32, bool> {
    let x: i32 = await fetch(input)?;
    let y: i32 = x + call(1);
    let z: Result<i32, bool> = Ok(y);
    let q: i32 = match z { Ok(v) => v, Err(e) => 0 };
    return q;
}
migration fn migrate() -> bool {
    old.get<i32>(store);
    finish_migration();
    return true;
}";
        let ast = parse_with_file(&lex(source).unwrap(), FileId(9)).unwrap();

        assert_eq!(ast.span.file, FileId(9));
        assert!(!ast.span.is_empty());
        assert!(!ast.imports[0].span.is_empty());
        assert!(!ast.types[0].span.is_empty());
        assert!(!ast.types[0].fields[0].span.is_empty());
        let AstType::BuiltinGeneric { arguments, .. } = ast.types[0].fields[0].ty.kind() else {
            panic!("field type should be generic");
        };
        assert!(!ast.types[0].fields[0].ty.span().is_empty());
        assert!(arguments.iter().all(|argument| !argument.span().is_empty()));
        assert!(!ast.types[1].variants[0].span.is_empty());

        let function = &ast.functions[0];
        assert!(!function.span.is_empty());
        assert!(!function.parameters[0].span.is_empty());
        assert!(!function.parameters[0].ty.span().is_empty());
        assert!(!function.result.span.is_empty());
        assert!(!function.result.ty.span().is_empty());
        assert!(
            function
                .body
                .iter()
                .all(|statement| !statement.span().is_empty())
        );

        let AstStatement::Bind { value, .. } = function.body[0].kind() else {
            panic!("expected binding");
        };
        let AstExpression::Await(awaited) = value.kind() else {
            panic!("expected await");
        };
        let AstExpression::Try(called) = awaited.kind() else {
            panic!("expected try");
        };
        assert!(matches!(called.kind(), AstExpression::Call { .. }));
        assert!(!value.span().is_empty());
        assert!(!awaited.span().is_empty());
        assert!(!called.span().is_empty());

        let AstStatement::Bind { value, .. } = function.body[1].kind() else {
            panic!("expected binding");
        };
        let AstExpression::Binary { op, rhs, .. } = value.kind() else {
            panic!("expected binary expression");
        };
        assert!(!op.span.is_empty());
        assert!(matches!(rhs.kind(), AstExpression::Call { .. }));

        let AstStatement::Bind { value, .. } = function.body[2].kind() else {
            panic!("expected binding");
        };
        assert!(matches!(value.kind(), AstExpression::Constructor { .. }));

        let AstStatement::Bind { value, .. } = function.body[3].kind() else {
            panic!("expected binding");
        };
        let AstExpression::Match { arms, .. } = value.kind() else {
            panic!("expected match");
        };
        assert!(arms.iter().all(|arm| !arm.span.is_empty()));

        let migration = &ast.functions[1];
        let AstStatement::Expression(expression) = migration.body[0].kind() else {
            panic!("expected migration expression");
        };
        let AstExpression::Migration(intrinsic) = expression.kind() else {
            panic!("expected migration intrinsic");
        };
        assert!(!intrinsic.span().is_empty());

        let hir_source = "fn identity(value: i32) -> i32 { return value; }";
        let hir_ast = parse_with_file(&lex(hir_source).unwrap(), FileId(11)).unwrap();
        let hir = resolve_and_typecheck(hir_ast).unwrap();
        assert_eq!(hir.span().file, FileId(11));
        assert!(hir.function_spans().all(|span| !span.is_empty()));
    }

    #[test]
    fn arithmetic_function_compiles_verifies_and_executes() {
        let module = compile(
            "fn add(a: i32, b: i32) -> i32 {
                return a + b;
            }",
        )
        .unwrap();
        let outcome = CheckedInterpreter::run(
            &module,
            0,
            &[RuntimeValue::I32(20), RuntimeValue::I32(22)],
            100,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(42)),
                ..
            }
        ));
    }

    #[test]
    fn every_scalar_type_compiles_verifies_and_executes_with_defined_arithmetic() {
        let module = compile(
            "fn wrap32() -> i32 { return 2147483647 + 1; }
             fn wrap64() -> i64 { return 9223372036854775807 + 1; }
             fn ratio32() -> f32 { return 7.5 / 2.5; }
             fn ratio64() -> f64 { return 7.0 / 2.0; }
             fn glyph() -> rune { return '界'; }
             fn divide_zero() -> i64 { return 9 / 0; }",
        )
        .unwrap();

        let returned = |function| {
            let InterpreterOutcome::Returned { value, .. } =
                CheckedInterpreter::run(&module, function, &[], 100).unwrap()
            else {
                panic!("scalar function must return");
            };
            value
        };
        assert_eq!(returned(0), Some(RuntimeValue::I32(i32::MIN)));
        assert_eq!(returned(1), Some(RuntimeValue::I64(i64::MIN)));
        assert_eq!(returned(2), Some(RuntimeValue::F32(3.0_f32.to_bits())));
        assert_eq!(returned(3), Some(RuntimeValue::F64(3.5_f64.to_bits())));
        assert_eq!(returned(4), Some(RuntimeValue::Rune('界' as u32)));

        let InterpreterOutcome::Trapped { trap, .. } =
            CheckedInterpreter::run(&module, 5, &[], 100).unwrap()
        else {
            panic!("integer division by zero must trap");
        };
        assert_eq!(trap.kind, TrapKind::DivideByZero);

        assert!(matches!(
            compile("fn narrow(value: i64) -> i32 { return value; }"),
            Err(CompileError::InvalidNumericConversion { .. })
        ));
    }

    #[test]
    fn immutable_utf8_strings_cover_literals_operations_and_rune_iteration() {
        let module = compile(
            "fn concat(value: string) -> string { return value + \"界\"; }
             fn rune_len() -> i32 { let value: string = \"a界\"; return value.len(); }
             fn byte_len() -> i32 { let value: string = \"a界\"; return value.byte_len(); }
             fn equal() -> bool { return \"same\" == \"same\"; }
             fn rune() -> rune { let value: string = \"a界\"; return value.rune_at(1); }
             fn hash() -> i64 { let value: string = \"key\"; return value.hash(); }
             fn invalid() -> rune { let value: string = \"a\"; return value.rune_at(1); }",
        )
        .unwrap();
        assert_eq!(module.module().strings, ["a", "a界", "key", "same", "界"]);
        let mut heap = nexa_runtime::Heap::new_with_string_limit(32, 32);
        let input = heap.allocate_string("Nexa").unwrap();
        let input_hash = heap.string_hash(input).unwrap();
        let InterpreterOutcome::Returned {
            value: Some(RuntimeValue::String {
                reference: result, ..
            }),
            ..
        } = CheckedInterpreter::run_with_heap(
            &module,
            0,
            &[RuntimeValue::String {
                reference: input,
                hash: input_hash,
            }],
            100,
            &mut heap,
        )
        .unwrap()
        else {
            panic!("concat must return a GC string");
        };
        assert_eq!(heap.string(result), Ok("Nexa界"));

        let returned = |function, heap: &mut nexa_runtime::Heap| {
            let InterpreterOutcome::Returned { value, .. } =
                CheckedInterpreter::run_with_heap(&module, function, &[], 100, heap).unwrap()
            else {
                panic!("string operation must return");
            };
            value
        };
        assert_eq!(returned(1, &mut heap), Some(RuntimeValue::I32(2)));
        assert_eq!(returned(2, &mut heap), Some(RuntimeValue::I32(4)));
        assert_eq!(returned(3, &mut heap), Some(RuntimeValue::Bool(true)));
        assert_eq!(
            returned(4, &mut heap),
            Some(RuntimeValue::Rune('界' as u32))
        );
        assert!(matches!(returned(5, &mut heap), Some(RuntimeValue::I64(_))));
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 6, &[], 100, &mut heap).unwrap(),
            InterpreterOutcome::Trapped {
                trap: nexa_runtime::Trap {
                    kind: TrapKind::StringIndexOutOfBounds,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn user_enum_payloads_construct_match_bind_and_nest_builtin_enums() {
        let source = "enum Failure { Rejected }
             enum Event {
                 Idle,
                 Damage(i32),
                 Deferred(Option<Result<i32, Failure>>),
             }
             fn damage(value: i32) -> Event {
                 return Damage(value);
             }
             fn read(value: Event) -> i32 {
                 return match value {
                     Idle => 0,
                     Damage(amount) => amount,
                     Deferred(pending) => match pending {
                         None => 1,
                         Some(result) => match result {
                             Ok(amount) => amount,
                             Err(error) => 2,
                         },
                     },
                 };
             }
             fn nested() -> Event {
                 let result: Result<i32, Failure> = Ok(9);
                 let pending: Option<Result<i32, Failure>> = Some(result);
                 return Deferred(pending);
             }
             fn idle() -> Event {
                 return Idle;
             }";
        let module = compile(source).unwrap();
        let event = module
            .module()
            .enum_types
            .iter()
            .find(|enum_type| enum_type.type_id == StableId::from_name("Event"))
            .unwrap();
        assert_eq!(
            event
                .variants
                .iter()
                .map(|variant| variant.payload_type)
                .collect::<Vec<_>>(),
            [
                None,
                Some(nexa_bytecode::ValueType::I32),
                Some(nexa_bytecode::ValueType::Named(
                    nexa_bytecode::option_type(nexa_bytecode::ValueType::Named(
                        nexa_bytecode::result_type(
                            nexa_bytecode::ValueType::I32,
                            nexa_bytecode::ValueType::Named(StableId::from_name("Failure")),
                        )
                        .type_id,
                    ))
                    .type_id,
                )),
            ]
        );

        let mut heap = nexa_runtime::Heap::new(16);
        let InterpreterOutcome::Returned {
            value: Some(damage),
            ..
        } = CheckedInterpreter::run_with_heap(&module, 0, &[RuntimeValue::I32(37)], 100, &mut heap)
            .unwrap()
        else {
            panic!("payload enum constructor must return");
        };
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[damage], 100, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(37)),
                ..
            }
        ));
        let InterpreterOutcome::Returned {
            value: Some(nested),
            ..
        } = CheckedInterpreter::run_with_heap(&module, 2, &[], 100, &mut heap).unwrap()
        else {
            panic!("nested enum constructor must return");
        };
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[nested], 100, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(9)),
                ..
            }
        ));
        let InterpreterOutcome::Returned {
            value: Some(idle), ..
        } = CheckedInterpreter::run_with_heap(&module, 3, &[], 100, &mut heap).unwrap()
        else {
            panic!("unit enum constructor must return");
        };
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[idle], 100, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(0)),
                ..
            }
        ));

        assert!(matches!(
            compile(
                "enum Event { Idle, Damage(i32) }
                 fn bad(value: Event) -> i32 {
                     return match value { Idle => 0 };
                 }"
            ),
            Err(CompileError::NonExhaustiveMatch)
        ));
        assert!(matches!(
            compile(
                "enum Event { Idle, Damage(i32) }
                 fn bad() -> Event { return Event.Damage(true); }"
            ),
            Err(CompileError::TypeMismatch)
        ));

        let idl = nexa_idl::parse(
            "interface Engine {
                 enum Event { Idle, Damage(i32) }
                 sync fn echo(value: Event) -> Event;
             }",
        )
        .unwrap();
        let host_module = super::compile_with_interface(
            "module game;
             import engine;
             fn echo_damage(value: i32) -> Event {
                 return engine.echo(Event.Damage(value));
             }",
            &idl,
            StableId::from_name("schema"),
        )
        .unwrap();
        let event = host_module
            .module()
            .enum_types
            .iter()
            .find(|enum_type| enum_type.type_id == StableId::from_name("Event"))
            .unwrap();
        assert_eq!(
            event.variants[1].payload_type,
            Some(nexa_bytecode::ValueType::I32)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn structs_are_nominal_immutable_structurally_equal_values() {
        let source = "struct Position { x: i32; y: i32; label: string; }
             enum Event { Idle, Move(Position) }
             fn make(x: i32, y: i32) -> Position {
                 return Position { label: \"origin\", y: y, x: x };
             }
             fn read_x(value: Position) -> i32 {
                 return value.x;
             }
             fn move_x(value: Position) -> Position {
                 return value with { x: value.x + 1, label: \"moved\" };
             }
             fn equal() -> bool {
                 let lhs: Position = Position { x: 1, y: 2, label: \"same\" };
                 let rhs: Position = Position { x: 1, y: 2, label: \"same\" };
                 return lhs == rhs;
             }
             fn event() -> Event {
                 return Move(Position { x: 3, y: 4, label: \"event\" });
             }
             fn event_y(value: Event) -> i32 {
                 return match value {
                     Idle => 0,
                     Move(position) => position.y,
                 };
             }";
        let module = compile(source).unwrap();
        let position = module
            .module()
            .struct_types
            .iter()
            .find(|struct_type| struct_type.type_id == StableId::from_name("Position"))
            .unwrap();
        assert_eq!(
            position
                .fields
                .iter()
                .map(|field| field.ty)
                .collect::<Vec<_>>(),
            [
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::String,
            ]
        );

        let mut heap = nexa_runtime::Heap::new_with_string_limit(32, 64);
        let InterpreterOutcome::Returned {
            value: Some(original),
            ..
        } = CheckedInterpreter::run_with_heap(
            &module,
            0,
            &[RuntimeValue::I32(7), RuntimeValue::I32(8)],
            200,
            &mut heap,
        )
        .unwrap()
        else {
            panic!("struct constructor must return");
        };
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[original], 100, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(7)),
                ..
            }
        ));
        let InterpreterOutcome::Returned {
            value: Some(updated),
            ..
        } = CheckedInterpreter::run_with_heap(&module, 2, &[original], 200, &mut heap).unwrap()
        else {
            panic!("struct with update must return");
        };
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[updated], 100, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(8)),
                ..
            }
        ));
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[original], 100, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(7)),
                ..
            }
        ));
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 3, &[], 300, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::Bool(true)),
                ..
            }
        ));
        let InterpreterOutcome::Returned {
            value: Some(event), ..
        } = CheckedInterpreter::run_with_heap(&module, 4, &[], 200, &mut heap).unwrap()
        else {
            panic!("enum struct payload must return");
        };
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 5, &[event], 200, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(4)),
                ..
            }
        ));

        assert!(matches!(
            compile(
                "struct Pair { left: i32; right: i32; }
                 fn bad() -> Pair { return Pair { left: 1 }; }"
            ),
            Err(CompileError::TypeMismatch)
        ));
        assert!(matches!(
            compile(
                "struct Pair { left: i32; right: i32; }
                 fn bad(value: Pair) -> i32 { return value.missing; }"
            ),
            Err(CompileError::TypeMismatch)
        ));
        assert!(matches!(
            compile(
                "struct Left { value: i32; }
                 struct Right { value: i32; }
                 fn bad(value: Left) -> Right { return value; }"
            ),
            Err(CompileError::TypeMismatch)
        ));

        let idl = nexa_idl::parse(
            "interface Geometry {
                 struct Position { x: i32; y: i32; label: string; }
                 sync fn echo(value: Position) -> Position;
             }",
        )
        .unwrap();
        let host_module = super::compile_with_interface(
            "module game;
             import geometry;
             fn echo_position(value: Position) -> Position {
                 return geometry.echo(value with { x: value.x + 1 });
             }",
            &idl,
            StableId::from_name("schema"),
        )
        .unwrap();
        assert_eq!(host_module.module().struct_types.len(), 1);
        assert!(
            host_module.module().functions[0]
                .code
                .iter()
                .any(|instruction| matches!(
                    instruction,
                    nexa_bytecode::Instruction::HostCall { .. }
                ))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn classes_are_mutable_identity_values_and_cycles_are_gc_managed() {
        let source = "class Node { value: i32; next: Option<Node>; }
             fn make(value: i32) -> Node {
                 return new Node { next: None, value: value };
             }
             fn mutate(node: Node, value: i32) -> i32 {
                 node.value = value;
                 return node.value;
             }
             fn same(lhs: Node, rhs: Node) -> bool {
                 return lhs == rhs;
             }
             fn cycle() -> Node {
                 let a: Node = new Node { value: 1, next: None };
                 let b: Node = new Node { value: 2, next: None };
                 a.next = Some(b);
                 b.next = Some(a);
                 return a;
             }
             fn next_value(node: Node) -> i32 {
                 return match node.next {
                     None => 0,
                     Some(next) => next.value,
                 };
             }";
        let module = compile(source).unwrap();
        let node_type = StableId::from_name("Node");
        assert_eq!(module.module().class_types.len(), 1);
        assert_eq!(module.module().class_types[0].type_id, node_type);

        let mut heap = nexa_runtime::Heap::new(32);
        let InterpreterOutcome::Returned {
            value: Some(first), ..
        } = CheckedInterpreter::run_with_heap(&module, 0, &[RuntimeValue::I32(3)], 200, &mut heap)
            .unwrap()
        else {
            panic!("class constructor must return");
        };
        let InterpreterOutcome::Returned {
            value: Some(second),
            ..
        } = CheckedInterpreter::run_with_heap(&module, 0, &[RuntimeValue::I32(3)], 200, &mut heap)
            .unwrap()
        else {
            panic!("second class constructor must return");
        };
        assert!(matches!(
            CheckedInterpreter::run_with_heap(
                &module,
                1,
                &[first, RuntimeValue::I32(9)],
                200,
                &mut heap
            )
            .unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(9)),
                ..
            }
        ));
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 2, &[first, first], 100, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::Bool(true)),
                ..
            }
        ));
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 2, &[first, second], 100, &mut heap)
                .unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::Bool(false)),
                ..
            }
        ));

        let InterpreterOutcome::Returned {
            value: Some(cycle), ..
        } = CheckedInterpreter::run_with_heap(&module, 3, &[], 500, &mut heap).unwrap()
        else {
            panic!("cyclic class graph must return");
        };
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 4, &[cycle], 200, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(2)),
                ..
            }
        ));
        let RuntimeValue::NamedRef { reference, .. } = cycle else {
            panic!("class values are nominal references");
        };
        let roots = nexa_runtime::GcRoots {
            running_frames: vec![reference],
            ..nexa_runtime::GcRoots::default()
        };
        let kept = heap.collect(&roots).unwrap();
        assert!(kept.live >= 4);
        let reclaimed = heap.collect(&nexa_runtime::GcRoots::default()).unwrap();
        assert!(reclaimed.reclaimed >= 4);

        assert!(matches!(
            compile(
                "class Left { value: i32; }
                 class Right { value: i32; }
                 fn bad(value: Left) -> Right { return value; }"
            ),
            Err(CompileError::TypeMismatch)
        ));
        assert!(matches!(
            compile(
                "class Node { value: i32; }
                 fn bad(node: Node) -> i32 {
                     node.value = true;
                     return node.value;
                 }"
            ),
            Err(CompileError::TypeMismatch)
        ));
        assert!(matches!(
            compile(
                "class Node { value: i32; }
                 fn bad() -> Node { return null; }"
            ),
            Err(CompileError::UnknownName(name)) if name == "null"
        ));
    }

    #[test]
    fn arrays_compile_verify_execute_and_trap_at_runtime_boundaries() {
        let module = compile(
            "fn array_ops() -> i32 {
                 let values: Array<i32> = Array.new<i32>();
                 values.push(10);
                 values.insert(0, 5);
                 values.set(1, 7);
                 let first: i32 = values.get(0);
                 let removed: i32 = values.remove(1);
                 values.push(9);
                 let popped: i32 = values.pop();
                 let length: i32 = values.len();
                 values.clear();
                 return first + removed + popped + length;
             }
             fn out_of_bounds() -> i32 {
                 let values: Array<i32> = Array.new<i32>();
                 return values.get(0);
             }",
        )
        .unwrap();
        assert_eq!(
            module.module().array_types,
            vec![nexa_bytecode::ArrayType::new(nexa_bytecode::ValueType::I32)]
        );
        let function = &module.module().functions[0];
        for (pc, instruction) in function.code.iter().enumerate() {
            if matches!(
                instruction,
                Instruction::ArrayNew { .. }
                    | Instruction::ArrayLen { .. }
                    | Instruction::ArrayGet { .. }
                    | Instruction::ArraySet { .. }
                    | Instruction::ArrayPush { .. }
                    | Instruction::ArrayPop { .. }
                    | Instruction::ArrayInsert { .. }
                    | Instruction::ArrayRemove { .. }
                    | Instruction::ArrayClear { .. }
            ) {
                let pc = u32::try_from(pc).unwrap();
                assert!(function.safepoints.contains(&pc));
                assert!(function.root_maps.iter().any(|root_map| root_map.pc == pc));
            }
        }

        let mut heap = nexa_runtime::Heap::new(16);
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 0, &[], 500, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(22)),
                ..
            }
        ));
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[], 100, &mut heap).unwrap(),
            InterpreterOutcome::Trapped {
                trap: nexa_runtime::Trap {
                    kind: TrapKind::ArrayIndexOutOfBounds,
                    ..
                },
                ..
            }
        ));

        let mut exhausted = nexa_runtime::Heap::new(0);
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 0, &[], 100, &mut exhausted),
            Err(nexa_runtime::InterpreterError::Heap(
                nexa_runtime::HeapError::CapacityExhausted
            ))
        ));

        for source in [
            "immediate fn bad() -> Array<i32> { return Array.new<i32>(); }",
            "immediate fn bad(values: Array<i32>) -> i32 { return values.len(); }",
        ] {
            assert_eq!(compile(source).unwrap_err(), CompileError::InvalidEffect);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn buffers_compile_verify_execute_and_preserve_copy_ownership() {
        let module = compile(
            "fn buffer_ops(destination: Buffer<i32>, source: Buffer<i32>) -> i32 {
                 destination.set(0, 7);
                 destination.copy(source, 0, 1, 2);
                 let part: Buffer<i32> = destination.slice(1, 2);
                 return destination.get(0) + part.get(0) + part.len();
             }
             fn out_of_bounds(buffer: Buffer<i32>) -> i32 {
                 return buffer.get(99);
             }",
        )
        .unwrap();
        let metadata = nexa_bytecode::BufferType::new(nexa_bytecode::ValueType::I32);
        assert_eq!(module.module().buffer_types, vec![metadata]);
        let function = &module.module().functions[0];
        for required in [
            "BufferLen",
            "BufferGet",
            "BufferSet",
            "BufferSlice",
            "BufferCopy",
        ] {
            assert!(
                function
                    .code
                    .iter()
                    .any(|instruction| format!("{instruction:?}").starts_with(required)),
                "missing {required}"
            );
        }
        for (pc, instruction) in function.code.iter().enumerate() {
            if matches!(
                instruction,
                Instruction::BufferLen { .. }
                    | Instruction::BufferGet { .. }
                    | Instruction::BufferSet { .. }
                    | Instruction::BufferSlice { .. }
                    | Instruction::BufferCopy { .. }
            ) {
                let pc = u32::try_from(pc).unwrap();
                assert!(function.safepoints.contains(&pc));
                assert!(function.root_maps.iter().any(|root_map| root_map.pc == pc));
            }
        }

        let mut heap = nexa_runtime::Heap::new_with_limits(8, usize::MAX, 8);
        let destination = heap
            .allocate_buffer(
                metadata.type_id,
                metadata.element,
                &[
                    RuntimeValue::I32(1),
                    RuntimeValue::I32(2),
                    RuntimeValue::I32(3),
                ],
            )
            .unwrap();
        let source = heap
            .allocate_buffer(
                metadata.type_id,
                metadata.element,
                &[
                    RuntimeValue::I32(9),
                    RuntimeValue::I32(8),
                    RuntimeValue::I32(7),
                ],
            )
            .unwrap();
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 0, &[destination, source], 500, &mut heap,)
                .unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(18)),
                ..
            }
        ));
        assert_eq!(
            heap.buffer_values(destination),
            Ok(&[
                RuntimeValue::I32(7),
                RuntimeValue::I32(9),
                RuntimeValue::I32(8),
            ][..])
        );
        assert_eq!(
            heap.buffer_values(source),
            Ok(&[
                RuntimeValue::I32(9),
                RuntimeValue::I32(8),
                RuntimeValue::I32(7),
            ][..])
        );
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[source], 100, &mut heap).unwrap(),
            InterpreterOutcome::Trapped {
                trap: nexa_runtime::Trap {
                    kind: TrapKind::BufferIndexOutOfBounds,
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            compile("immediate fn bad(buffer: Buffer<i32>) -> i32 { return buffer.len(); }")
                .unwrap_err(),
            CompileError::InvalidEffect
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn maps_compile_verify_execute_and_cover_every_source_operation() {
        let module = compile(
            "fn map_ops() -> i32 {
                 let values: Map<i32, string> = Map.new<i32, string>();
                 values.set(1, \"one\");
                 values.set(2, \"two\");
                 let found: Option<string> = values.get(1);
                 let removed: Option<string> = values.remove(2);
                 let contains: bool = values.contains(1);
                 let length: i32 = values.len();
                 values.clear();
                 return match found {
                     Some(value) => value.byte_len() + length,
                     None => 0,
                 };
             }
             fn missing() -> i32 {
                 let values: Map<i32, string> = Map.new<i32, string>();
                 return match values.get(9) {
                     Some(value) => value.byte_len(),
                     None => 0,
                 };
             }",
        )
        .unwrap();
        assert_eq!(
            module.module().map_types,
            vec![nexa_bytecode::MapType::new(
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::String,
            )]
        );
        let code = &module.module().functions[0].code;
        for required in [
            "MapNew",
            "MapLen",
            "MapGet",
            "MapSet",
            "MapRemove",
            "MapContains",
            "MapClear",
        ] {
            assert!(
                code.iter()
                    .any(|instruction| format!("{instruction:?}").starts_with(required)),
                "missing {required}"
            );
        }
        let function = &module.module().functions[0];
        for (pc, instruction) in function.code.iter().enumerate() {
            if matches!(
                instruction,
                Instruction::MapNew { .. }
                    | Instruction::MapLen { .. }
                    | Instruction::MapGet { .. }
                    | Instruction::MapSet { .. }
                    | Instruction::MapRemove { .. }
                    | Instruction::MapContains { .. }
                    | Instruction::MapClear { .. }
            ) {
                let pc = u32::try_from(pc).unwrap();
                assert!(function.safepoints.contains(&pc));
                assert!(function.root_maps.iter().any(|root_map| root_map.pc == pc));
            }
        }

        let mut heap = nexa_runtime::Heap::new_with_limits(32, 64, 16);
        let outcome = CheckedInterpreter::run_with_heap(&module, 0, &[], 1_000, &mut heap).unwrap();
        assert!(
            matches!(
                outcome,
                InterpreterOutcome::Returned {
                    value: Some(RuntimeValue::I32(4)),
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[], 300, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(0)),
                ..
            }
        ));

        let remove = compile(
            "fn remove(values: Map<i32, i32>) -> Option<i32> {
                 return values.remove(1);
             }",
        )
        .unwrap();
        let mut full_heap = nexa_runtime::Heap::new_with_limits(1, usize::MAX, 8);
        let map_type =
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I32);
        let map = full_heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::I32,
            )
            .unwrap();
        assert_eq!(
            full_heap
                .map_set(map, RuntimeValue::I32(1), RuntimeValue::I32(7))
                .unwrap(),
            nexa_runtime::MapSetOutcome::Complete
        );
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&remove, 0, &[map], 100, &mut full_heap),
            Err(nexa_runtime::InterpreterError::Heap(
                nexa_runtime::HeapError::CapacityExhausted
            ))
        ));
        assert_eq!(full_heap.map_contains(map, RuntimeValue::I32(1)), Ok(true));

        for source in [
            "fn bad() -> Map<bool, i32> { return Map.new<bool, i32>(); }",
            "fn bad() -> Map<f32, i32> { return Map.new<f32, i32>(); }",
            "class Key { value: i32; }
             fn bad() -> Map<Key, i32> { return Map.new<Key, i32>(); }",
            "immediate fn bad() -> Map<i32, i32> { return Map.new<i32, i32>(); }",
            "immediate fn bad(values: Map<i32, i32>) -> i32 { return values.len(); }",
        ] {
            assert!(matches!(
                compile(source),
                Err(CompileError::TypeMismatch | CompileError::InvalidEffect)
            ));
        }
        assert!(
            compile(
                "@stateful class Store { value: i32; }
                 fn keyed(
                     values: Map<StateHandle<Store>, i32>,
                     handle: StateHandle<Store>
                 ) -> bool {
                     values.set(handle, 1);
                     return values.contains(handle);
                 }
                 fn scalar_keys() -> bool {
                     let a: Map<i64, i32> = Map.new<i64, i32>();
                     let b: Map<rune, i32> = Map.new<rune, i32>();
                     let c: Map<string, i32> = Map.new<string, i32>();
                     return true;
                 }"
            )
            .is_ok()
        );
    }

    #[test]
    fn register_planner_handles_nested_eight_argument_calls_without_fixed_slack() {
        let module = compile(
            "fn id(value: i32) -> i32 {
                return value;
             }
             fn sum8(
                a: i32, b: i32, c: i32, d: i32,
                e: i32, f: i32, g: i32, h: i32
             ) -> i32 {
                return a + b + c + d + e + f + g + h;
             }
             fn nested(value: i32) -> i32 {
                return sum8(
                    1, 2, 3, 4, 5, 6, 7,
                    sum8(value, 1, 2, 3, 4, 5, 6, 7)
                );
             }",
        )
        .unwrap();
        assert_eq!(module.module().functions[0].registers, 2);
        assert!(module.module().functions[2].registers >= 18);
        assert!(matches!(
            CheckedInterpreter::run(&module, 2, &[RuntimeValue::I32(10)], 1_000).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(66)),
                ..
            }
        ));
    }

    #[test]
    fn register_planner_covers_eight_argument_host_match_try_and_defer_windows() {
        for arity in 0..=8 {
            let parameters = (0..arity)
                .map(|index| format!("p{index}: i32"))
                .collect::<Vec<_>>()
                .join(", ");
            let arguments = (0..arity)
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let idl = nexa_idl::parse(&format!(
                "interface Engine {{ sync fn probe({parameters}) -> i32; }}"
            ))
            .unwrap();
            let module = super::compile_with_interface(
                &format!(
                    "module game.registers;
                     import engine;
                     fn call() -> i32 {{ return engine.probe({arguments}); }}"
                ),
                &idl,
                StableId::from_name("schema"),
            )
            .unwrap();
            assert!(module.module().functions[0].registers > arity);
        }

        let module = compile(
            "enum Failure { Rejected }
             fn sum8(
                a: i32, b: i32, c: i32, d: i32,
                e: i32, f: i32, g: i32, h: i32
             ) -> i32 {
                return a + b + c + d + e + f + g + h;
             }
             fn in_match(value: Option<i32>) -> i32 {
                return match value {
                    Some(found) => sum8(found, 1, 2, 3, 4, 5, 6, 7),
                    None => 0,
                };
             }
             fn in_try(value: Result<i32, Failure>) -> Result<i32, Failure> {
                return Ok(sum8(value?, 1, 2, 3, 4, 5, 6, 7));
             }
             fn in_defer() -> i32 {
                defer sum8(1, 2, 3, 4, 5, 6, 7, 8);
                return 0;
             }",
        )
        .unwrap();
        assert!(module.module().functions[1].registers >= 9);
        assert!(module.module().functions[2].registers >= 9);
        assert!(module.module().functions[3].registers >= 9);
    }

    #[test]
    fn control_flow_task_await_and_reference_roots_close_the_pipeline() {
        let choose = compile(
            "fn choose(flag: bool, a: i32, b: i32) -> i32 {
                if flag { return a; } else { return b; }
            }",
        )
        .unwrap();
        let outcome = CheckedInterpreter::run(
            &choose,
            0,
            &[
                RuntimeValue::Bool(false),
                RuntimeValue::I32(1),
                RuntimeValue::I32(2),
            ],
            100,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(2)),
                ..
            }
        ));

        let reference = compile(
            "@stateful class Entity { score: i32; }
             struct Pair { left: i32; right: i32; }
             enum Mode { Idle, Running }
             fn identity(value: Entity) -> Entity {
                return value;
            }",
        )
        .unwrap();
        let value = RuntimeValue::NamedRef {
            reference: GcRef {
                index: 3,
                generation: 4,
            },
            type_id: StableId::from_name("Entity"),
        };
        assert!(matches!(
            CheckedInterpreter::run(&reference, 0, &[value], 100).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(result),
                ..
            } if result == value
        ));

        let task = compile(
            "task fn id(value: i32) -> i32 { return value; }
             task fn update(value: i32) -> i32 { return await id(value); }",
        )
        .unwrap();
        assert!(matches!(
            CheckedInterpreter::run(&task, 1, &[RuntimeValue::I32(8)], 100).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(8)),
                ..
            }
        ));
    }

    #[test]
    fn qualified_host_import_emits_typed_host_call_without_synthetic_yield() {
        let idl = nexa_idl::parse(
            "interface Engine {
                enum AnimationError { MissingClip, Interrupted, Cancelled }
                request(return_error, trap) fn animation(entity: i32)
                    -> request<Result<i32, AnimationError>>;
            }",
        )
        .unwrap();
        let source = "module game.combat;
             import engine;
             task fn update(entity: i32) -> i32 {
                 await engine.animation(entity);
                 return entity;
             }";
        let module =
            super::compile_with_interface(source, &idl, StableId::from_name("schema")).unwrap();
        assert_eq!(module.module().host_imports.len(), 1);
        assert!(
            module.module().functions[0]
                .code
                .iter()
                .any(|instruction| matches!(
                    instruction,
                    nexa_bytecode::Instruction::HostCall { import: 0, .. }
                ))
        );
        assert!(
            !module.module().functions[0]
                .code
                .iter()
                .any(|instruction| matches!(instruction, nexa_bytecode::Instruction::Yield))
        );
        let host_pc = module.module().functions[0]
            .code
            .iter()
            .position(|instruction| {
                matches!(
                    instruction,
                    nexa_bytecode::Instruction::HostCall { import: 0, .. }
                )
            })
            .unwrap();
        let host_span = module
            .module()
            .source_span(0, u32::try_from(host_pc).unwrap())
            .unwrap();
        assert_eq!(
            &source[host_span.start as usize..host_span.end as usize],
            "engine.animation(entity)"
        );
        let resume_span = module
            .module()
            .source_span(0, u32::try_from(host_pc + 1).unwrap())
            .unwrap();
        assert_eq!(
            &source[resume_span.start as usize..resume_span.end as usize],
            "await engine.animation(entity)"
        );
        assert!((0..module.module().functions[0].code.len()).all(|pc| {
            module
                .module()
                .source_span(0, u32::try_from(pc).unwrap())
                .is_some()
        }));
    }

    #[test]
    fn source_map_covers_calls_matches_enums_migration_and_safepoints() {
        let source = "enum Failure { Bad }
            fn increment(value: i32) -> i32 { return value + 1; }
            fn inspect(value: Option<i32>) -> i32 {
                return match value {
                    Some(found) => increment(found),
                    None => 0,
                };
            }
            fn wrap(value: i32) -> Option<i32> { return Some(value); }
            migration fn migrate() -> bool {
                old.get<i32>(legacy);
                finish_migration();
                return true;
            }";
        let module = compile(source).unwrap();
        let module = module.module();
        let mut required_instruction_count = 0_usize;
        for (function_index, function) in module.functions.iter().enumerate() {
            for (pc, instruction) in function.code.iter().enumerate() {
                if matches!(
                    instruction,
                    Instruction::Call { .. }
                        | Instruction::EnumNew { .. }
                        | Instruction::EnumTag { .. }
                        | Instruction::EnumPayload { .. }
                        | Instruction::JumpIfFalse { .. }
                        | Instruction::StateOldGet { .. }
                        | Instruction::StateFinish
                        | Instruction::Safepoint
                ) {
                    required_instruction_count += 1;
                    let span = module
                        .source_span(
                            u32::try_from(function_index).unwrap(),
                            u32::try_from(pc).unwrap(),
                        )
                        .unwrap();
                    assert_eq!(span.file, FileId(0));
                    assert!(!span.is_empty());
                    assert!(span.end as usize <= source.len());
                }
            }
        }
        assert!(required_instruction_count >= 8);
    }

    #[test]
    fn effects_flow_scopes_defer_and_nominal_types_are_enforced() {
        assert_eq!(
            compile(
                "fn id(value: i32) -> i32 { return value; }
                 fn bad(value: i32) -> i32 { return await id(value); }"
            )
            .unwrap_err(),
            CompileError::InvalidEffect
        );
        let immediate = compile("immediate fn one() -> i32 { return 1; }").unwrap();
        assert_eq!(
            immediate.module().functions[0].effect,
            FunctionEffect::Immediate
        );
        assert_eq!(
            compile(
                "fn id(value: i32) -> i32 { return value; }
                 cleanup fn bad(value: i32) -> i32 { return await id(value); }"
            )
            .unwrap_err(),
            CompileError::InvalidEffect
        );
        assert_eq!(
            compile("fn partial(flag: bool) -> i32 { if flag { return 1; } }").unwrap_err(),
            CompileError::MissingReturn
        );
        assert!(matches!(
            compile(
                "fn scoped(flag: bool) -> i32 {
                    if flag { let hidden = 1; hidden; }
                    return hidden;
                 }"
            ),
            Err(CompileError::UnknownName(name)) if name == "hidden"
        ));
        assert_eq!(
            compile(
                "struct A { value: i32; }
                 struct B { value: i32; }
                 fn take(value: A) -> A { return value; }
                 fn bad(value: B) -> A { return take(value); }"
            )
            .unwrap_err(),
            CompileError::TypeMismatch
        );

        let shadow = compile(
            "fn shadow() -> i32 {
                let value = 1;
                if true { let value = 2; value; }
                return value;
             }",
        )
        .unwrap();
        assert!(matches!(
            CheckedInterpreter::run(&shadow, 0, &[], 100).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(1)),
                ..
            }
        ));

        let deferred = compile(
            "fn finalize(value: i32) -> i32 { return value; }
             fn run(value: i32) -> i32 {
                defer finalize(value);
                return value;
             }",
        )
        .unwrap();
        assert!(matches!(
            deferred.module().functions[1].code.as_slice(),
            [
                nexa_bytecode::Instruction::Move { .. },
                nexa_bytecode::Instruction::DeferPush { .. },
                ..,
                nexa_bytecode::Instruction::Return { .. }
            ]
        ));
        assert!(matches!(
            CheckedInterpreter::run(&deferred, 1, &[RuntimeValue::I32(5)], 100).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(5)),
                ..
            }
        ));
    }

    #[test]
    fn compiler_emits_reload_entries_and_rejects_ambiguous_metadata() {
        let module = compile(
            "@stateful class Store { value: i32; }
             migration fn migrate() -> bool {
                 finish_migration();
                 return true;
             }
             @activation fn activate() -> i32 {
                 return 1;
             }",
        )
        .unwrap();
        let metadata = module.module().reload_metadata;
        assert_eq!(metadata.migration_entry, Some(0));
        assert_eq!(metadata.activation_entry, Some(1));
        assert_eq!(
            metadata.stateful_schema_hash,
            module.module().state_schema.stable_hash()
        );
        assert_eq!(
            module.module().functions[0].effect,
            FunctionEffect::Migration
        );
        assert_eq!(
            module.module().functions[1].effect,
            FunctionEffect::Immediate
        );
        assert!(metadata.minimum_migration_limits.max_fuel > 0);
        assert_eq!(metadata.minimum_migration_limits.max_call_depth, 1);

        assert_eq!(
            compile(
                "migration fn first() -> bool { return true; }
                 migration fn second() -> bool { return true; }"
            )
            .unwrap_err(),
            CompileError::InvalidReloadMetadata("multiple migration entries")
        );
        assert_eq!(
            compile(
                "@activation fn first() -> bool { return true; }
                 @activation fn second() -> bool { return true; }"
            )
            .unwrap_err(),
            CompileError::InvalidReloadMetadata("multiple activation entries")
        );
        assert_eq!(
            compile("@activation task fn invalid() -> bool { return true; }").unwrap_err(),
            CompileError::InvalidReloadMetadata("activation entry must have Immediate effect")
        );
    }

    #[test]
    fn state_handle_generic_methods_compile_to_verified_typed_opcodes() {
        let module = compile(
            "@stateful class EnemyBrain { phase: i32; }
             fn inspect(
                 handle: StateHandle<EnemyBrain>,
                 other: StateHandle<EnemyBrain>
             ) -> i32 {
                 let resolved: Result<EnemyBrain, StateHandleError> = handle.resolve();
                 let alive: bool = handle.is_alive();
                 let id: StableId = handle.stable_id();
                 let generation: i32 = handle.generation();
                 let equal: bool = handle.equality(other);
                 return handle.hash();
             }",
        )
        .unwrap();
        let code = &module.module().functions[0].code;
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateHandleResolve { .. }))
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateHandleIsAlive { .. }))
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateHandleStableId { .. }))
        );
        assert!(
            code.iter().any(|instruction| matches!(
                instruction,
                Instruction::StateHandleGeneration { .. }
            ))
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateHandleEqual { .. }))
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateHandleHash { .. }))
        );
    }

    #[test]
    fn stateful_classes_enforce_persistent_field_closure_and_typed_migration() {
        let module = compile(
            "struct Stats { score: i64; label: string; }
             enum Phase { Idle, Active(i32) }
             @stateful class Entity {
                 stats: Stats;
                 phase: Phase;
                 next: StateHandle<Entity>;
                 optional_next: Option<StateHandle<Entity>>;
             }
             migration fn migrate() -> bool {
                 let entity: Entity = new.create<Entity>(entity);
                 new.set(entity, Entity.phase, Idle);
                 preserve(kept);
                 replace(replaced, entity);
                 delete(removed);
                 finish_migration();
                 return true;
             }",
        )
        .unwrap();
        let entity = StableId::from_name("Entity");
        assert_eq!(module.module().state_schema.types.len(), 1);
        assert!(
            module
                .module()
                .state_handle_types
                .iter()
                .any(|handle| handle.target == nexa_bytecode::ValueType::Named(entity))
        );
        let code = &module.module().functions[0].code;
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateNewCreate { .. }))
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateNewSet { .. }))
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StatePreserve { .. }))
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateReplace { .. }))
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateDelete { .. }))
        );
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateFinish))
        );

        for source in [
            "class Mutable { value: i32; }
             @stateful class Store { forbidden: Mutable; }",
            "class Mutable { value: i32; }
             struct Wrapper { forbidden: Mutable; }
             @stateful class Store { nested: Wrapper; }",
            "class Mutable { value: i32; }
             enum Wrapper { Empty, Forbidden(Mutable) }
             @stateful class Store { nested: Wrapper; }",
            "class Mutable { value: i32; }
             @stateful class Store { nested: Option<Mutable>; }",
            "class Mutable { value: i32; }
             @stateful class Store { forged: StateHandle<Mutable>; }",
            "class Mutable { value: i32; }
             fn forged(handle: StateHandle<Mutable>) -> bool { return true; }",
            "@stateful class Store { request: HostRequest; }",
            "@stateful class Store { token: ResourceToken; }",
            "@stateful class Store { snapshot: Snapshot; }",
            "@stateful class Store { buffer: Buffer; }",
        ] {
            assert_eq!(compile(source).unwrap_err(), CompileError::TypeMismatch);
        }

        assert_eq!(
            compile(
                "@stateful class Store { value: i32; }
                 migration fn migrate() -> bool {
                     let store: Store = new.create<Store>(store);
                     new.set(store, Store.value, true);
                     finish_migration();
                     return true;
                 }"
            )
            .unwrap_err(),
            CompileError::TypeMismatch
        );
    }
}
