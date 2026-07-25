//! Minimal staged Nexa compiler: lex, parse, resolve/type-check, HIR and verified bytecode.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nexa_bytecode::{
    AbandonPolicy, AsyncResultType, CancelPolicy, EnumType, EnumVariant, Function, FunctionEffect,
    HostCallMode, HostImport, Instruction, Module, ModuleBuilder, RootMap, ScriptExport, Signature,
    StateField, StateSchema, StateType, ValueType,
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
    Integer(i32),
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
    Equal,
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
pub struct CompileDiagnostic {
    pub message: String,
    pub span: SourceSpan,
}

impl CompileError {
    #[must_use]
    pub fn diagnostic(&self, file: FileId) -> CompileDiagnostic {
        let (start, end) = match self {
            Self::UnexpectedCharacter { offset, character } => {
                (*offset, offset.saturating_add(character.len_utf8()))
            }
            Self::UnexpectedToken { offset, .. } => (*offset, offset.saturating_add(1)),
            _ => (0, 0),
        };
        CompileDiagnostic {
            message: self.to_string(),
            span: SourceSpan::new(
                file,
                u32::try_from(start).unwrap_or(u32::MAX),
                u32::try_from(end).unwrap_or(u32::MAX),
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstModule {
    pub name: Option<String>,
    pub imports: Vec<String>,
    pub types: Vec<AstTypeDeclaration>,
    pub functions: Vec<AstFunction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstTypeDeclaration {
    pub name: String,
    pub kind: AstTypeKind,
    pub version: u32,
    pub fields: Vec<(String, AstType)>,
    pub variants: Vec<String>,
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
    pub parameters: Vec<(String, AstType)>,
    pub result: AstType,
    pub body: Vec<AstStatement>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AstType {
    I32,
    Bool,
    Named(String),
    BuiltinGeneric { name: String, arguments: Vec<Self> },
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AstExpression {
    Integer(i32),
    Bool(bool),
    Name(String),
    Binary {
        op: BinaryOp,
        lhs: Box<Self>,
        rhs: Box<Self>,
    },
    Call {
        function: String,
        arguments: Vec<Self>,
    },
    Await(Box<Self>),
    Constructor {
        variant: BuiltinVariant,
        payload: Option<Box<Self>>,
    },
    Match {
        value: Box<Self>,
        arms: Vec<MatchArm>,
    },
    Try(Box<Self>),
    Migration(MigrationIntrinsic),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinVariant {
    None,
    Some,
    Ok,
    Err,
}

impl BuiltinVariant {
    const fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Some => "Some",
            Self::Ok => "Ok",
            Self::Err => "Err",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchArm {
    pub variant: String,
    pub binding: Option<String>,
    pub value: AstExpression,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
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

pub fn lex(source: &str) -> Result<Vec<Token>, CompileError> {
    let mut tokens = Vec::new();
    let mut chars = source.char_indices().peekable();
    while let Some((offset, character)) = chars.next() {
        if character.is_whitespace() {
            continue;
        }
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
            '=' if chars.peek().is_some_and(|(_, next)| *next == '>') => {
                chars.next();
                TokenKind::FatArrow
            }
            '=' => TokenKind::Equal,
            '<' => TokenKind::Less,
            '>' => TokenKind::Greater,
            '?' => TokenKind::Question,
            '@' => TokenKind::At,
            '.' if chars.peek().is_some_and(|(_, next)| *next == '.') => {
                chars.next();
                TokenKind::DotDot
            }
            '.' => TokenKind::Dot,
            '-' if chars.peek().is_some_and(|(_, next)| *next == '>') => {
                chars.next();
                TokenKind::Arrow
            }
            '-' => TokenKind::Minus,
            digit if digit.is_ascii_digit() => {
                let mut text = digit.to_string();
                while let Some((_, next)) = chars.peek() {
                    if !next.is_ascii_digit() {
                        break;
                    }
                    text.push(*next);
                    chars.next();
                }
                TokenKind::Integer(text.parse().map_err(|_| CompileError::UnexpectedToken {
                    offset,
                    expected: "i32 integer",
                })?)
            }
            first if first == '_' || first.is_ascii_alphabetic() => {
                let mut text = first.to_string();
                while let Some((_, next)) = chars.peek() {
                    if *next != '_' && !next.is_ascii_alphanumeric() {
                        break;
                    }
                    text.push(*next);
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
        tokens.push(Token { kind, offset });
    }
    Ok(tokens)
}

pub fn parse(tokens: &[Token]) -> Result<AstModule, CompileError> {
    Parser { tokens, cursor: 0 }.module()
}

struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
}

impl Parser<'_> {
    fn module(mut self) -> Result<AstModule, CompileError> {
        let name = if self.take(&TokenKind::Module) {
            let name = self.qualified_ident()?;
            self.expect(&TokenKind::Semicolon, ";")?;
            Some(name)
        } else {
            None
        };
        let mut imports = Vec::new();
        while self.take(&TokenKind::Import) {
            imports.push(self.qualified_ident()?);
            self.expect(&TokenKind::Semicolon, ";")?;
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
        })
    }

    fn type_declaration(&mut self) -> Result<AstTypeDeclaration, CompileError> {
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
            let member = self.ident()?;
            if kind == AstTypeKind::Enum {
                variants.push(member);
                self.take(&TokenKind::Comma);
            } else {
                self.expect(&TokenKind::Colon, ":")?;
                fields.push((member, self.ty()?));
                self.expect(&TokenKind::Semicolon, ";")?;
            }
        }
        Ok(AstTypeDeclaration {
            name,
            kind,
            version,
            fields,
            variants,
        })
    }

    fn function(&mut self) -> Result<AstFunction, CompileError> {
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
                let parameter = self.ident()?;
                self.expect(&TokenKind::Colon, ":")?;
                parameters.push((parameter, self.ty()?));
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen, ")")?;
        self.expect(&TokenKind::Arrow, "->")?;
        let result = self.ty()?;
        let body = self.block()?;
        Ok(AstFunction {
            name,
            is_task,
            is_activation,
            effect,
            parameters,
            result,
            body,
        })
    }

    fn statement(&mut self) -> Result<AstStatement, CompileError> {
        if self.take(&TokenKind::Return) {
            let expression = self.expression(0)?;
            self.expect(&TokenKind::Semicolon, ";")?;
            return Ok(AstStatement::Return(expression));
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
            return Ok(AstStatement::Bind { name, ty, value });
        }
        if self.take(&TokenKind::If) {
            let condition = self.expression(0)?;
            let then_body = self.block()?;
            let else_body = if self.take(&TokenKind::Else) {
                self.block()?
            } else {
                Vec::new()
            };
            return Ok(AstStatement::If {
                condition,
                then_body,
                else_body,
            });
        }
        if self.take(&TokenKind::For) {
            let variable = self.ident()?;
            self.expect(&TokenKind::In, "in")?;
            let TokenKind::Integer(start) = self.next_kind()? else {
                return Err(self.unexpected("static range start"));
            };
            self.expect(&TokenKind::DotDot, "..")?;
            let TokenKind::Integer(end) = self.next_kind()? else {
                return Err(self.unexpected("static range end"));
            };
            if end < start || end.saturating_sub(start) > 1_024 {
                return Err(CompileError::InvalidEffect);
            }
            let body = self.block()?;
            let mut expanded = Vec::new();
            for value in start..end {
                let mut iteration = body.clone();
                substitute_name_in_statements(&mut iteration, &variable, value);
                expanded.extend(iteration);
            }
            return Ok(AstStatement::If {
                condition: AstExpression::Bool(true),
                then_body: expanded,
                else_body: Vec::new(),
            });
        }
        if self.take(&TokenKind::While) {
            let condition = self.expression(0)?;
            return Ok(AstStatement::While {
                condition,
                body: self.block()?,
            });
        }
        if self.take(&TokenKind::Defer) {
            let expression = self.expression(0)?;
            self.expect(&TokenKind::Semicolon, ";")?;
            return Ok(AstStatement::Defer(expression));
        }
        let expression = self.expression(0)?;
        self.expect(&TokenKind::Semicolon, ";")?;
        Ok(AstStatement::Expression(expression))
    }

    #[allow(clippy::too_many_lines)]
    fn expression(&mut self, minimum_precedence: u8) -> Result<AstExpression, CompileError> {
        let mut lhs = match self.next_kind()? {
            TokenKind::Await => AstExpression::Await(Box::new(self.expression(3)?)),
            TokenKind::Match => self.match_expression()?,
            TokenKind::Integer(value) => AstExpression::Integer(value),
            TokenKind::True => AstExpression::Bool(true),
            TokenKind::False => AstExpression::Bool(false),
            TokenKind::Ident(mut name) => {
                while self.take(&TokenKind::Dot) {
                    name.push('.');
                    name.push_str(&self.ident()?);
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
                if is_migration_intrinsic(&name) {
                    AstExpression::Migration(self.migration_intrinsic(&name, type_arguments)?)
                } else if name == "None" && type_arguments.is_empty() {
                    AstExpression::Constructor {
                        variant: BuiltinVariant::None,
                        payload: None,
                    }
                } else if matches!(name.as_str(), "Some" | "Ok" | "Err")
                    && type_arguments.is_empty()
                {
                    self.expect(&TokenKind::LParen, "(")?;
                    let payload = self.expression(0)?;
                    self.expect(&TokenKind::RParen, ")")?;
                    AstExpression::Constructor {
                        variant: match name.as_str() {
                            "Some" => BuiltinVariant::Some,
                            "Ok" => BuiltinVariant::Ok,
                            "Err" => BuiltinVariant::Err,
                            _ => unreachable!(),
                        },
                        payload: Some(Box::new(payload)),
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
                    AstExpression::Name(name)
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
            if self.take(&TokenKind::Question) {
                lhs = AstExpression::Try(Box::new(lhs));
                continue;
            }
            let (precedence, op) = match self.peek_kind() {
                Some(TokenKind::Plus) => (1, BinaryOp::Add),
                Some(TokenKind::Minus) => (1, BinaryOp::Subtract),
                Some(TokenKind::Star) => (2, BinaryOp::Multiply),
                _ => break,
            };
            if precedence < minimum_precedence {
                break;
            }
            self.cursor += 1;
            let rhs = self.expression(precedence + 1)?;
            lhs = AstExpression::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn match_expression(&mut self) -> Result<AstExpression, CompileError> {
        let value = self.expression(0)?;
        self.expect(&TokenKind::LBrace, "{")?;
        let mut arms = Vec::new();
        while !self.take(&TokenKind::RBrace) {
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

    fn migration_intrinsic(
        &mut self,
        name: &str,
        type_arguments: Vec<AstType>,
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
        Ok(intrinsic)
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
        let name = self.ident()?;
        let base = match name.as_str() {
            "i32" => AstType::I32,
            "bool" => AstType::Bool,
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
            Ok(AstType::BuiltinGeneric { name, arguments })
        } else {
            Ok(base)
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

fn split_field_name(name: &str) -> Result<(String, String), CompileError> {
    name.rsplit_once('.')
        .map(|(owner, field)| (owner.to_owned(), field.to_owned()))
        .ok_or(CompileError::TypeMismatch)
}

fn migration_expressions(intrinsic: &MigrationIntrinsic) -> Vec<&AstExpression> {
    match intrinsic {
        MigrationIntrinsic::OldFieldGet { object, .. } => vec![object],
        MigrationIntrinsic::NewSet { object, value, .. } => vec![object, value],
        MigrationIntrinsic::Replace { target, .. } => vec![target],
        MigrationIntrinsic::OldGet { .. }
        | MigrationIntrinsic::NewCreate { .. }
        | MigrationIntrinsic::Preserve { .. }
        | MigrationIntrinsic::Delete { .. }
        | MigrationIntrinsic::Finish => Vec::new(),
    }
}

fn migration_expressions_mut(intrinsic: &mut MigrationIntrinsic) -> Vec<&mut AstExpression> {
    match intrinsic {
        MigrationIntrinsic::OldFieldGet { object, .. } => vec![object],
        MigrationIntrinsic::NewSet { object, value, .. } => vec![object, value],
        MigrationIntrinsic::Replace { target, .. } => vec![target],
        MigrationIntrinsic::OldGet { .. }
        | MigrationIntrinsic::NewCreate { .. }
        | MigrationIntrinsic::Preserve { .. }
        | MigrationIntrinsic::Delete { .. }
        | MigrationIntrinsic::Finish => Vec::new(),
    }
}

fn substitute_name_in_statements(statements: &mut [AstStatement], name: &str, value: i32) {
    for statement in statements {
        match statement {
            AstStatement::Bind {
                value: expression, ..
            }
            | AstStatement::Return(expression)
            | AstStatement::Expression(expression)
            | AstStatement::Defer(expression) => substitute_name(expression, name, value),
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
        }
    }
}

fn substitute_name(expression: &mut AstExpression, name: &str, value: i32) {
    match expression {
        AstExpression::Name(current) if current == name => {
            *expression = AstExpression::Integer(value);
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
        AstExpression::Integer(_) | AstExpression::Bool(_) | AstExpression::Name(_) => {}
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
        _ => StableId::from_name(name),
    }
}

fn collect_builtin_enum_types(ast: &AstModule, enum_types: &mut Vec<EnumType>) {
    let mut add_type = |ty: &AstType| collect_builtin_enum_type(ty, enum_types);
    for declaration in &ast.types {
        for (_, ty) in &declaration.fields {
            add_type(ty);
        }
    }
    for function in &ast.functions {
        for (_, ty) in &function.parameters {
            add_type(ty);
        }
        add_type(&function.result);
        collect_statement_builtin_types(&function.body, &mut add_type);
    }
}

fn collect_statement_builtin_types(
    statements: &[AstStatement],
    add_type: &mut impl FnMut(&AstType),
) {
    for statement in statements {
        match statement {
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
            | AstStatement::Defer(_) => {}
        }
    }
}

fn collect_builtin_enum_type(ty: &AstType, enum_types: &mut Vec<EnumType>) {
    let AstType::BuiltinGeneric { name, arguments } = ty else {
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
                    stable_id: StableId::from_parts(&[&declaration.name, "::", variant]),
                    tag: u32::try_from(tag).expect("enum variant count is parser bounded"),
                    payload_type: None,
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
    let mut enum_variants = BTreeMap::new();
    for declaration in ast
        .types
        .iter()
        .filter(|declaration| declaration.kind == AstTypeKind::Enum)
    {
        let type_id = StableId::from_name(&declaration.name);
        for (name, variant) in declaration.variants.iter().zip(
            enum_types
                .iter()
                .find(|enum_type| enum_type.type_id == type_id)
                .expect("source enum metadata was built")
                .variants
                .iter(),
        ) {
            enum_variants.insert((type_id, name.clone()), variant.clone());
        }
    }
    for enum_type in &enum_types {
        for variant in &enum_type.variants {
            for name in ["None", "Some", "Ok", "Err"] {
                if variant.stable_id == builtin_variant_id(name) {
                    enum_variants.insert((enum_type.type_id, name.to_owned()), variant.clone());
                }
            }
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
                    .map(|(name, ty)| StateField {
                        stable_id: StableId::from_parts(&[&declaration.name, "::", name]),
                        ty: lower_type(ty),
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
        for (_, ty) in &declaration.fields {
            validate_type(ty, &known_types)?;
        }
    }
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
        for (_, ty) in &function.parameters {
            validate_type(ty, &known_types)?;
        }
        validate_type(&function.result, &known_types)?;
        validate_statement_types(&function.body, &known_types)?;
        let signature = Signature {
            parameters: function
                .parameters
                .iter()
                .map(|(_, ty)| lower_type(ty))
                .collect(),
            result: Some(lower_type(&function.result)),
        };
        if signatures
            .insert(function.name.clone(), signature)
            .is_some()
        {
            return Err(CompileError::DuplicateName(function.name.clone()));
        }
    }
    let mut functions = Vec::new();
    for mut function in ast.functions {
        validate_awaits(&function.body, &suspending_functions)?;
        resolve_local_scopes(&mut function)?;
        let signature = signatures[&function.name].clone();
        let mut locals = BTreeMap::new();
        for (index, ((name, _), ty)) in function
            .parameters
            .iter()
            .zip(signature.parameters.iter().copied())
            .enumerate()
        {
            let register = u16::try_from(index).map_err(|_| CompileError::TooManyRegisters)?;
            if locals.insert(name.clone(), (register, ty)).is_some() {
                return Err(CompileError::DuplicateName(name.clone()));
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
            function_result: signature.result.expect("result is required"),
            effect: function.effect,
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
    })
}

fn validate_awaits(
    statements: &[AstStatement],
    suspending_functions: &BTreeSet<String>,
) -> Result<(), CompileError> {
    for statement in statements {
        match statement {
            AstStatement::Bind { value, .. }
            | AstStatement::Return(value)
            | AstStatement::Expression(value)
            | AstStatement::Defer(value) => {
                validate_await_expression(value, suspending_functions, false)?;
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
        }
    }
    Ok(())
}

fn validate_await_expression(
    expression: &AstExpression,
    suspending_functions: &BTreeSet<String>,
    awaited: bool,
) -> Result<(), CompileError> {
    match expression {
        AstExpression::Await(inner) => {
            let awaited = match inner.as_ref() {
                AstExpression::Try(expression) => expression.as_ref(),
                expression => expression,
            };
            let AstExpression::Call { function, .. } = awaited else {
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
        AstExpression::Integer(_) | AstExpression::Bool(_) | AstExpression::Name(_) => Ok(()),
    }
}

fn resolve_local_scopes(function: &mut AstFunction) -> Result<(), CompileError> {
    let mut root = BTreeMap::new();
    for (name, _) in &function.parameters {
        if root.insert(name.clone(), name.clone()).is_some() {
            return Err(CompileError::DuplicateName(name.clone()));
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
        match statement {
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
        }
    }
    Ok(())
}

fn resolve_expression(
    expression: &mut AstExpression,
    scopes: &mut Vec<BTreeMap<String, String>>,
    next_local: &mut u32,
) -> Result<(), CompileError> {
    match expression {
        AstExpression::Name(name) => {
            let resolved = scopes
                .iter()
                .rev()
                .find_map(|scope| scope.get(name))
                .cloned()
                .ok_or_else(|| CompileError::UnknownName(name.clone()))?;
            *name = resolved;
        }
        AstExpression::Binary { lhs, rhs, .. } => {
            resolve_expression(lhs, scopes, next_local)?;
            resolve_expression(rhs, scopes, next_local)?;
        }
        AstExpression::Call { arguments, .. } => {
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
        AstExpression::Integer(_) | AstExpression::Bool(_) => {}
    }
    Ok(())
}

fn validate_type(ty: &AstType, known_types: &BTreeSet<String>) -> Result<(), CompileError> {
    match ty {
        AstType::Named(name) if !known_types.contains(name) => {
            Err(CompileError::UnknownType(name.clone()))
        }
        AstType::BuiltinGeneric { name, arguments } => {
            let expected = match name.as_str() {
                "Option" => 1,
                "Result" => 2,
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
        AstType::I32 | AstType::Bool | AstType::Named(_) => Ok(()),
    }
}

fn validate_statement_types(
    statements: &[AstStatement],
    known_types: &BTreeSet<String>,
) -> Result<(), CompileError> {
    for statement in statements {
        match statement {
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
            | AstStatement::Defer(_) => {}
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
    function_result: ValueType,
    effect: FunctionEffect,
}

fn check_statements(
    statements: &[AstStatement],
    locals: &mut BTreeMap<String, (u16, ValueType)>,
    context: &TypeContext<'_>,
    next_register: &mut u16,
) -> Result<Flow, CompileError> {
    let mut flow = Flow::FallsThrough;
    for statement in statements {
        match statement {
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
                if matches!(condition, AstExpression::Bool(true)) && body_flow != Flow::FallsThrough
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
        }
    }
    Ok(flow)
}

fn statements_contain_await(statements: &[AstStatement]) -> bool {
    statements.iter().any(|statement| match statement {
        AstStatement::Bind { value, .. }
        | AstStatement::Return(value)
        | AstStatement::Expression(value)
        | AstStatement::Defer(value) => contains_await(value),
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
    })
}

fn contains_await(expression: &AstExpression) -> bool {
    match expression {
        AstExpression::Await(_) => true,
        AstExpression::Binary { lhs, rhs, .. } => contains_await(lhs) || contains_await(rhs),
        AstExpression::Call { arguments, .. } => arguments.iter().any(contains_await),
        AstExpression::Constructor { payload, .. } => {
            payload.as_deref().is_some_and(contains_await)
        }
        AstExpression::Match { value, arms } => {
            contains_await(value) || arms.iter().any(|arm| contains_await(&arm.value))
        }
        AstExpression::Try(expression) => contains_await(expression),
        AstExpression::Migration(intrinsic) => migration_expressions(intrinsic)
            .into_iter()
            .any(contains_await),
        AstExpression::Integer(_) | AstExpression::Bool(_) | AstExpression::Name(_) => false,
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
    let actual = match expression {
        AstExpression::Integer(_) => ValueType::I32,
        AstExpression::Bool(_) => ValueType::Bool,
        AstExpression::Name(name) => locals
            .get(name)
            .map(|(_, ty)| *ty)
            .ok_or_else(|| CompileError::UnknownName(name.clone()))?,
        AstExpression::Binary { lhs, rhs, .. } => {
            if expression_type(lhs, locals, context, next_register, Some(ValueType::I32))?
                != ValueType::I32
                || expression_type(rhs, locals, context, next_register, Some(ValueType::I32))?
                    != ValueType::I32
            {
                return Err(CompileError::TypeMismatch);
            }
            ValueType::I32
        }
        AstExpression::Call {
            function,
            arguments,
        } => {
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
        AstExpression::Constructor { variant, payload } => {
            let ValueType::Named(type_id) = expected.ok_or(CompileError::CannotInferType)? else {
                return Err(CompileError::TypeMismatch);
            };
            let metadata = context
                .enum_variants
                .get(&(type_id, variant.name().to_owned()))
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
    };
    if expected.is_some_and(|expected| actual != expected) {
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
    match intrinsic {
        MigrationIntrinsic::OldGet { ty, .. }
        | MigrationIntrinsic::NewCreate { ty, .. }
        | MigrationIntrinsic::OldFieldGet { ty, .. } => {
            if let MigrationIntrinsic::OldFieldGet { object, owner, .. } = intrinsic
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
            value,
            ..
        } => {
            expression_type(
                object,
                locals,
                context,
                next_register,
                Some(ValueType::Named(StableId::from_name(owner))),
            )?;
            expression_type(value, locals, context, next_register, None)?;
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
    }
}

fn lower_type(ty: &AstType) -> ValueType {
    match ty {
        AstType::I32 => ValueType::I32,
        AstType::Bool => ValueType::Bool,
        AstType::Named(name) => ValueType::Named(StableId::from_name(name)),
        AstType::BuiltinGeneric { name, arguments } if name == "Option" => {
            ValueType::Named(nexa_bytecode::option_type(lower_type(&arguments[0])).type_id)
        }
        AstType::BuiltinGeneric { name, arguments } if name == "Result" => ValueType::Named(
            nexa_bytecode::result_type(lower_type(&arguments[0]), lower_type(&arguments[1]))
                .type_id,
        ),
        AstType::BuiltinGeneric { .. } => unreachable!("generic types are validated"),
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
        match statement {
            AstStatement::Bind { value, .. }
            | AstStatement::Return(value)
            | AstStatement::Expression(value)
            | AstStatement::Defer(value) => inspect_expression_registers(value, plan)?,
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
    match expression {
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
        AstExpression::Integer(_) | AstExpression::Bool(_) | AstExpression::Name(_) => {}
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
    let required = match expression {
        AstExpression::Integer(_) | AstExpression::Bool(_) | AstExpression::Name(_) => 1,
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
        AstExpression::Match { value, arms } => {
            let mut required = usize::from(temporary_requirement(value)?).max(4);
            for arm in arms {
                required = required.max(usize::from(temporary_requirement(&arm.value)?));
            }
            required
        }
        AstExpression::Try(expression) => usize::from(temporary_requirement(expression)?).max(4),
        AstExpression::Migration(intrinsic) => match intrinsic {
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
        },
    };
    u16::try_from(required).map_err(|_| CompileError::TooManyRegisters)
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
    let mut host_functions = hir.host_functions.values().collect::<Vec<_>>();
    host_functions.sort_by_key(|function| function.import);
    for function in host_functions {
        module.host_import(function.metadata.clone());
    }
    if let (Some(host), Some(schema)) = (hir.host_interface_hash, hir.schema_hash) {
        module.metadata(host, schema);
    }
    module.state_schema(hir.state_schema.clone());
    for enum_type in &hir.enum_types {
        module.enum_type(enum_type.clone());
    }
    for function in &hir.functions {
        let mut code = Vec::new();
        let register_plan = plan_registers(function)?;
        let temporary = register_plan.local_count;
        let emit_context = EmitContext {
            functions: &function_ids,
            script_functions: &hir.functions,
            host_functions: &hir.host_functions,
            enum_variants: &hir.enum_variants,
            function_result: function.signature.result.expect("result is required"),
        };
        emit_statements(
            &function.body,
            temporary,
            &function.locals,
            &emit_context,
            &mut code,
        )?;
        let registers = register_plan.total;
        let safepoints = collect_safepoints(&code);
        let (root_bitmap, root_maps) =
            exact_root_maps(function, &code, &safepoints, hir, registers)?;
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
            code,
        });
    }
    let mut module = module.finish();
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
        nexa_bytecode::minimum_migration_limits(&module, module.reload_metadata.migration_entry);
    Ok(module)
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
            | Instruction::Mul { dst, .. } => state[usize::from(dst)] = Some(ValueType::I32),
            Instruction::LoadBool { dst, .. } | Instruction::CompareEq { dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::Bool);
            }
            Instruction::Move { dst, source } => {
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
            | Instruction::EnumNew { type_id, dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::Named(type_id));
            }
            Instruction::EnumTag { dst, .. } => {
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
    function_result: ValueType,
}

#[allow(clippy::too_many_lines)]
fn emit_statements(
    statements: &[AstStatement],
    temporary: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut Vec<Instruction>,
) -> Result<(), CompileError> {
    for statement in statements {
        match statement {
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
            AstStatement::Defer(AstExpression::Call {
                function,
                arguments,
            }) => {
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
            AstStatement::Defer(_) => return Err(CompileError::SuspendingDefer),
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
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn emit_expression(
    expression: &AstExpression,
    destination: u16,
    expected: Option<ValueType>,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
    code: &mut Vec<Instruction>,
) -> Result<(), CompileError> {
    match expression {
        AstExpression::Integer(value) => code.push(Instruction::LoadI32 {
            dst: destination,
            value: *value,
        }),
        AstExpression::Bool(value) => code.push(Instruction::LoadBool {
            dst: destination,
            value: *value,
        }),
        AstExpression::Name(name) => {
            let source = locals
                .get(name)
                .ok_or_else(|| CompileError::UnknownName(name.clone()))?
                .0;
            code.push(Instruction::Move {
                dst: destination,
                source,
            });
        }
        AstExpression::Binary { op, lhs, rhs } => {
            let lhs_register = destination;
            let rhs_register = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(
                lhs,
                lhs_register,
                Some(ValueType::I32),
                locals,
                context,
                code,
            )?;
            emit_expression(
                rhs,
                rhs_register,
                Some(ValueType::I32),
                locals,
                context,
                code,
            )?;
            code.push(match op {
                BinaryOp::Add => Instruction::Add {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                BinaryOp::Subtract => Instruction::Sub {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
                BinaryOp::Multiply => Instruction::Mul {
                    dst: destination,
                    lhs: lhs_register,
                    rhs: rhs_register,
                },
            });
        }
        AstExpression::Call {
            function,
            arguments,
        } => {
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
        }
        AstExpression::Constructor { variant, payload } => {
            let ValueType::Named(type_id) = expected.ok_or(CompileError::CannotInferType)? else {
                return Err(CompileError::TypeMismatch);
            };
            let metadata = context
                .enum_variants
                .get(&(type_id, variant.name().to_owned()))
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

fn emitted_expression_type(
    expression: &AstExpression,
    expected: Option<ValueType>,
    locals: &BTreeMap<String, (u16, ValueType)>,
    context: &EmitContext<'_>,
) -> Result<ValueType, CompileError> {
    match expression {
        AstExpression::Integer(_) | AstExpression::Binary { .. } => Ok(ValueType::I32),
        AstExpression::Bool(_) => Ok(ValueType::Bool),
        AstExpression::Name(name) => locals
            .get(name)
            .map(|(_, ty)| *ty)
            .ok_or_else(|| CompileError::UnknownName(name.clone())),
        AstExpression::Call { function, .. } => context
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
            .ok_or_else(|| CompileError::UnknownName(function.clone())),
        AstExpression::Await(expression) => {
            emitted_expression_type(expression, expected, locals, context)
        }
        AstExpression::Constructor { .. } | AstExpression::Match { .. } => {
            expected.ok_or(CompileError::CannotInferType)
        }
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
        AstExpression::Migration(intrinsic) => Ok(match intrinsic {
            MigrationIntrinsic::OldGet { ty, .. }
            | MigrationIntrinsic::OldFieldGet { ty, .. }
            | MigrationIntrinsic::NewCreate { ty, .. } => lower_type(ty),
            MigrationIntrinsic::NewSet { .. }
            | MigrationIntrinsic::Preserve { .. }
            | MigrationIntrinsic::Replace { .. }
            | MigrationIntrinsic::Delete { .. }
            | MigrationIntrinsic::Finish => ValueType::Bool,
        }),
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
    code: &mut Vec<Instruction>,
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
    code: &mut Vec<Instruction>,
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
    code: &mut Vec<Instruction>,
) -> Result<(), CompileError> {
    match intrinsic {
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
            emit_expression(
                object,
                destination,
                Some(ValueType::Named(StableId::from_name(owner))),
                locals,
                context,
                code,
            )?;
            let value_register = destination
                .checked_add(1)
                .ok_or(CompileError::TooManyRegisters)?;
            emit_expression(value, value_register, None, locals, context, code)?;
            code.push(Instruction::StateNewSet {
                object: destination,
                field_id: StableId::from_parts(&[owner, "::", field]),
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
                    | Instruction::Yield
                    | Instruction::Call { .. }
                    | Instruction::HostCall { .. }
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
                variants: enumeration.variants.clone(),
            });
        }
    }
    let import = ast
        .imports
        .first()
        .and_then(|name| name.rsplit('.').next())
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
                        enumeration
                            .variants
                            .iter()
                            .position(|candidate| candidate == variant)
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
        TypeRef::Bool => ValueType::Bool,
        TypeRef::HostRequest(_) => ValueType::Named(StableId::from_name("HostRequest")),
        TypeRef::ResourceToken(_) => ValueType::Named(StableId::from_name("ResourceToken")),
        TypeRef::Snapshot(_) => ValueType::Named(StableId::from_name("Snapshot")),
        TypeRef::Option(inner) => {
            ValueType::Named(nexa_bytecode::option_type(lower_idl_type(inner)).type_id)
        }
        TypeRef::Result(success, error) => ValueType::Named(
            nexa_bytecode::result_type(lower_idl_type(success), lower_idl_type(error)).type_id,
        ),
        TypeRef::F32 => ValueType::Named(StableId::from_name("f32")),
        TypeRef::String => ValueType::Named(StableId::from_name("string")),
        TypeRef::Named(name) => ValueType::Named(StableId::from_name(name)),
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
    use nexa_bytecode::FunctionEffect;
    use nexa_core::StableId;
    use nexa_runtime::{CheckedInterpreter, GcRef, InterpreterOutcome, RuntimeValue};

    use super::{CompileError, compile};

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
        let module = super::compile_with_interface(
            "module game.combat;
             import engine;
             task fn update(entity: i32) -> i32 {
                 await engine.animation(entity);
                 return entity;
             }",
            &idl,
            StableId::from_name("schema"),
        )
        .unwrap();
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
}
