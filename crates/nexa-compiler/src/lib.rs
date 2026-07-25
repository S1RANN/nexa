//! Minimal staged Nexa compiler: lex, parse, resolve/type-check, HIR and verified bytecode.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nexa_bytecode::{
    Function, FunctionEffect, HostCallMode, HostImport, Instruction, Module, ModuleBuilder,
    RootMap, ScriptExport, Signature, StateField, StateSchema, StateType, ValueType,
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
    Cleanup,
    Return,
    Let,
    Var,
    If,
    Else,
    While,
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
    MissingReturn,
    SuspendingDefer,
    DeferCaptureLimit,
    InvalidEffect,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AstStatement {
    Bind {
        name: String,
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
    effect: FunctionEffect,
    signature: Signature,
    body: Vec<AstStatement>,
    locals: BTreeMap<String, (u16, ValueType)>,
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
            '=' => TokenKind::Equal,
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
                    "cleanup" => TokenKind::Cleanup,
                    "return" => TokenKind::Return,
                    "let" => TokenKind::Let,
                    "var" => TokenKind::Var,
                    "if" => TokenKind::If,
                    "else" => TokenKind::Else,
                    "while" => TokenKind::While,
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
            if matches!(
                self.peek_kind(),
                Some(TokenKind::Struct | TokenKind::Enum | TokenKind::Class | TokenKind::At)
            ) {
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
        let effect = if self.take(&TokenKind::Task) {
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
            self.expect(&TokenKind::Equal, "=")?;
            let value = self.expression(0)?;
            self.expect(&TokenKind::Semicolon, ";")?;
            return Ok(AstStatement::Bind { name, value });
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

    fn expression(&mut self, minimum_precedence: u8) -> Result<AstExpression, CompileError> {
        let mut lhs = match self.next_kind()? {
            TokenKind::Await => AstExpression::Await(Box::new(self.expression(3)?)),
            TokenKind::Integer(value) => AstExpression::Integer(value),
            TokenKind::True => AstExpression::Bool(true),
            TokenKind::False => AstExpression::Bool(false),
            TokenKind::Ident(mut name) => {
                while self.take(&TokenKind::Dot) {
                    name.push('.');
                    name.push_str(&self.ident()?);
                }
                if self.take(&TokenKind::LParen) {
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

    fn block(&mut self) -> Result<Vec<AstStatement>, CompileError> {
        self.expect(&TokenKind::LBrace, "{")?;
        let mut body = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            body.push(self.statement()?);
        }
        Ok(body)
    }

    fn ty(&mut self) -> Result<AstType, CompileError> {
        Ok(match self.ident()?.as_str() {
            "i32" => AstType::I32,
            "bool" => AstType::Bool,
            named => AstType::Named(named.to_owned()),
        })
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
        AstExpression::Await(expression) => substitute_name(expression, name, value),
        AstExpression::Integer(_) | AstExpression::Bool(_) | AstExpression::Name(_) => {}
    }
}

pub fn resolve_and_typecheck(ast: AstModule) -> Result<HirModule, CompileError> {
    resolve_and_typecheck_with_hosts(ast, BTreeMap::new(), None, None)
}

#[allow(clippy::too_many_lines)]
fn resolve_and_typecheck_with_hosts(
    ast: AstModule,
    host_functions: BTreeMap<String, HostFunction>,
    host_interface_hash: Option<StableId>,
    schema_hash: Option<StableId>,
) -> Result<HirModule, CompileError> {
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
        let flow = check_statements(
            &function.body,
            &mut locals,
            &signatures,
            signature.result.expect("result is required"),
            &mut next_register,
        )?;
        if flow == Flow::FallsThrough {
            return Err(CompileError::MissingReturn);
        }
        functions.push(HirFunction {
            name: function.name,
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
            let AstExpression::Call { function, .. } = inner.as_ref() else {
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
            AstStatement::Bind { name, value } => {
                resolve_expression(value, scopes)?;
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
            | AstStatement::Defer(expression) => resolve_expression(expression, scopes)?,
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                resolve_expression(condition, scopes)?;
                scopes.push(BTreeMap::new());
                resolve_statements(then_body, scopes, next_local)?;
                scopes.pop();
                scopes.push(BTreeMap::new());
                resolve_statements(else_body, scopes, next_local)?;
                scopes.pop();
            }
            AstStatement::While { condition, body } => {
                resolve_expression(condition, scopes)?;
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
    scopes: &[BTreeMap<String, String>],
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
            resolve_expression(lhs, scopes)?;
            resolve_expression(rhs, scopes)?;
        }
        AstExpression::Call { arguments, .. } => {
            for argument in arguments {
                resolve_expression(argument, scopes)?;
            }
        }
        AstExpression::Await(expression) => resolve_expression(expression, scopes)?,
        AstExpression::Integer(_) | AstExpression::Bool(_) => {}
    }
    Ok(())
}

fn validate_type(ty: &AstType, known_types: &BTreeSet<String>) -> Result<(), CompileError> {
    if let AstType::Named(name) = ty
        && !known_types.contains(name)
    {
        return Err(CompileError::UnknownType(name.clone()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    FallsThrough,
    Returns,
    Diverges,
}

fn check_statements(
    statements: &[AstStatement],
    locals: &mut BTreeMap<String, (u16, ValueType)>,
    signatures: &BTreeMap<String, Signature>,
    result: ValueType,
    next_register: &mut u16,
) -> Result<Flow, CompileError> {
    let mut flow = Flow::FallsThrough;
    for statement in statements {
        match statement {
            AstStatement::Bind { name, value } => {
                let ty = expression_type(value, locals, signatures)?;
                let register = *next_register;
                *next_register = next_register
                    .checked_add(1)
                    .ok_or(CompileError::TooManyRegisters)?;
                if locals.insert(name.clone(), (register, ty)).is_some() {
                    return Err(CompileError::DuplicateName(name.clone()));
                }
            }
            AstStatement::Return(expression) => {
                if expression_type(expression, locals, signatures)? != result {
                    return Err(CompileError::TypeMismatch);
                }
                flow = Flow::Returns;
            }
            AstStatement::Expression(expression) => {
                expression_type(expression, locals, signatures)?;
            }
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                if expression_type(condition, locals, signatures)? != ValueType::Bool {
                    return Err(CompileError::TypeMismatch);
                }
                let mut then_locals = locals.clone();
                let mut else_locals = locals.clone();
                let then_flow = check_statements(
                    then_body,
                    &mut then_locals,
                    signatures,
                    result,
                    next_register,
                )?;
                let else_flow = check_statements(
                    else_body,
                    &mut else_locals,
                    signatures,
                    result,
                    next_register,
                )?;
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
                if expression_type(condition, locals, signatures)? != ValueType::Bool {
                    return Err(CompileError::TypeMismatch);
                }
                let mut loop_locals = locals.clone();
                let body_flow =
                    check_statements(body, &mut loop_locals, signatures, result, next_register)?;
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
                expression_type(expression, locals, signatures)?;
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
        AstExpression::Integer(_) | AstExpression::Bool(_) | AstExpression::Name(_) => false,
    }
}

fn expression_type(
    expression: &AstExpression,
    locals: &BTreeMap<String, (u16, ValueType)>,
    signatures: &BTreeMap<String, Signature>,
) -> Result<ValueType, CompileError> {
    match expression {
        AstExpression::Integer(_) => Ok(ValueType::I32),
        AstExpression::Bool(_) => Ok(ValueType::Bool),
        AstExpression::Name(name) => locals
            .get(name)
            .map(|(_, ty)| *ty)
            .ok_or_else(|| CompileError::UnknownName(name.clone())),
        AstExpression::Binary { lhs, rhs, .. } => {
            if expression_type(lhs, locals, signatures)? == ValueType::I32
                && expression_type(rhs, locals, signatures)? == ValueType::I32
            {
                Ok(ValueType::I32)
            } else {
                Err(CompileError::TypeMismatch)
            }
        }
        AstExpression::Call {
            function,
            arguments,
        } => {
            let signature = signatures
                .get(function)
                .ok_or_else(|| CompileError::UnknownName(function.clone()))?;
            if arguments.len() != signature.parameters.len() {
                return Err(CompileError::TypeMismatch);
            }
            for (argument, expected) in arguments.iter().zip(&signature.parameters) {
                if expression_type(argument, locals, signatures)? != *expected {
                    return Err(CompileError::TypeMismatch);
                }
            }
            Ok(signature.result.expect("functions have results"))
        }
        AstExpression::Await(expression) => expression_type(expression, locals, signatures),
    }
}

fn lower_type(ty: &AstType) -> ValueType {
    match ty {
        AstType::I32 => ValueType::I32,
        AstType::Bool => ValueType::Bool,
        AstType::Named(name) => ValueType::Named(StableId::from_name(name)),
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
    let mut host_functions = hir.host_functions.values().collect::<Vec<_>>();
    host_functions.sort_by_key(|function| function.import);
    for function in host_functions {
        module.host_import(function.metadata.clone());
    }
    if let (Some(host), Some(schema)) = (hir.host_interface_hash, hir.schema_hash) {
        module.metadata(host, schema);
    }
    module.state_schema(hir.state_schema.clone());
    for function in &hir.functions {
        let mut code = Vec::new();
        let temporary =
            u16::try_from(function.locals.len()).map_err(|_| CompileError::TooManyRegisters)?;
        emit_statements(
            &function.body,
            temporary,
            &function.locals,
            &function_ids,
            &hir.host_functions,
            &mut code,
        )?;
        let registers =
            u16::try_from(function.locals.len() + 8).map_err(|_| CompileError::TooManyRegisters)?;
        let mut root_bitmap = vec![false; usize::from(registers)];
        for (register, ty) in function.locals.values() {
            root_bitmap[usize::from(*register)] = ty.is_reference();
        }
        if function
            .signature
            .result
            .is_some_and(ValueType::is_reference)
        {
            root_bitmap[function.locals.len()] = true;
        }
        let mut changed = true;
        while changed {
            changed = false;
            for instruction in &code {
                let (destination, is_reference) = match instruction {
                    Instruction::Move { dst, source } => (*dst, root_bitmap[usize::from(*source)]),
                    Instruction::Call {
                        function: callee,
                        dst,
                        ..
                    } => (
                        *dst,
                        hir.functions[*callee as usize]
                            .signature
                            .result
                            .is_some_and(ValueType::is_reference),
                    ),
                    Instruction::HostCall { import, dst, .. } => (
                        *dst,
                        hir.host_functions
                            .values()
                            .find(|function| function.import == *import)
                            .and_then(|function| function.metadata.result)
                            .is_some_and(ValueType::is_reference),
                    ),
                    _ => continue,
                };
                if is_reference && !root_bitmap[usize::from(destination)] {
                    root_bitmap[usize::from(destination)] = true;
                    changed = true;
                }
            }
        }
        let safepoints = collect_safepoints(&code);
        let root_maps = exact_root_maps(function, &code, &safepoints, hir)?;
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
    Ok(module.finish())
}

#[allow(clippy::too_many_lines)]
fn exact_root_maps(
    function: &HirFunction,
    code: &[Instruction],
    safepoints: &[u32],
    module: &HirModule,
) -> Result<Vec<RootMap>, CompileError> {
    use std::collections::VecDeque;

    let register_count = function.locals.len() + 8;
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
            Instruction::StateOldGet { ty, dst, .. } => {
                state[usize::from(dst)] = Some(ty);
            }
            Instruction::StateNewCreate { type_id, dst, .. } => {
                state[usize::from(dst)] = Some(ValueType::Named(type_id));
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
            | Instruction::StateHandleRemap { .. }
            | Instruction::StateDelete { .. }
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
    Ok(safepoints
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
        .collect())
}

#[allow(clippy::too_many_lines)]
fn emit_statements(
    statements: &[AstStatement],
    temporary: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    functions: &BTreeMap<String, u32>,
    host_functions: &BTreeMap<String, HostFunction>,
    code: &mut Vec<Instruction>,
) -> Result<(), CompileError> {
    for statement in statements {
        match statement {
            AstStatement::Bind { name, value } => {
                emit_expression(
                    value,
                    locals[name].0,
                    locals,
                    functions,
                    host_functions,
                    code,
                )?;
            }
            AstStatement::Return(value) => {
                emit_expression(value, temporary, locals, functions, host_functions, code)?;
                code.push(Instruction::Return { source: temporary });
            }
            AstStatement::Expression(expression) => {
                emit_expression(
                    expression,
                    temporary,
                    locals,
                    functions,
                    host_functions,
                    code,
                )?;
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
                        locals,
                        functions,
                        host_functions,
                        code,
                    )?;
                }
                code.push(Instruction::DeferPush {
                    function: *functions
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
                    locals,
                    functions,
                    host_functions,
                    code,
                )?;
                let branch = code.len();
                code.push(Instruction::JumpIfFalse {
                    condition: temporary,
                    target: 0,
                });
                emit_statements(
                    then_body,
                    temporary,
                    locals,
                    functions,
                    host_functions,
                    code,
                )?;
                let skip_else = code.len();
                code.push(Instruction::Jump { target: 0 });
                let else_start =
                    u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
                code[branch] = Instruction::JumpIfFalse {
                    condition: temporary,
                    target: else_start,
                };
                emit_statements(
                    else_body,
                    temporary,
                    locals,
                    functions,
                    host_functions,
                    code,
                )?;
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
                    locals,
                    functions,
                    host_functions,
                    code,
                )?;
                let exit = code.len();
                code.push(Instruction::JumpIfFalse {
                    condition: temporary,
                    target: 0,
                });
                emit_statements(body, temporary, locals, functions, host_functions, code)?;
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

fn emit_expression(
    expression: &AstExpression,
    destination: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    functions: &BTreeMap<String, u32>,
    host_functions: &BTreeMap<String, HostFunction>,
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
            emit_expression(lhs, lhs_register, locals, functions, host_functions, code)?;
            emit_expression(rhs, rhs_register, locals, functions, host_functions, code)?;
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
            for (index, argument) in arguments.iter().enumerate() {
                emit_expression(
                    argument,
                    args_base
                        .checked_add(
                            u16::try_from(index).map_err(|_| CompileError::TooManyRegisters)?,
                        )
                        .ok_or(CompileError::TooManyRegisters)?,
                    locals,
                    functions,
                    host_functions,
                    code,
                )?;
            }
            let args_count =
                u16::try_from(arguments.len()).map_err(|_| CompileError::TooManyRegisters)?;
            if let Some(host) = host_functions.get(function) {
                code.push(Instruction::HostCall {
                    import: host.import,
                    args_base,
                    args_count,
                    dst: destination,
                });
            } else {
                code.push(Instruction::Call {
                    function: *functions
                        .get(function)
                        .ok_or_else(|| CompileError::UnknownName(function.clone()))?,
                    args_base,
                    args_count,
                    dst: destination,
                });
            }
        }
        AstExpression::Await(expression) => {
            emit_expression(
                expression,
                destination,
                locals,
                functions,
                host_functions,
                code,
            )?;
        }
    }
    Ok(())
}

fn collect_safepoints(code: &[Instruction]) -> Vec<u32> {
    code.iter()
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
        .collect()
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

pub fn compile_with_interface(
    source: &str,
    interface: &Idl,
    schema_hash: StableId,
) -> Result<VerifiedModule, CompileError> {
    let tokens = lex(source)?;
    let ast = parse(&tokens)?;
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
        let result = Some(lower_idl_type(&function.result));
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
                request fn animation(entity: i32) -> host_request;
            }",
        )
        .unwrap();
        let module = super::compile_with_interface(
            "module game.combat;
             import engine;
             task fn update(entity: i32) -> HostRequest {
                 return await engine.animation(entity);
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
}
