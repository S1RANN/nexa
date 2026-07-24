//! Minimal staged Nexa compiler: lex, parse, resolve/type-check, HIR and verified bytecode.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nexa_bytecode::{Function, Instruction, Module, ModuleBuilder, Signature, ValueType};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Fn,
    Task,
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
    pub types: Vec<AstTypeDeclaration>,
    pub functions: Vec<AstFunction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstTypeDeclaration {
    pub name: String,
    pub kind: AstTypeKind,
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
}

#[derive(Clone, Debug)]
struct HirFunction {
    name: String,
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
        Ok(AstModule { types, functions })
    }

    fn type_declaration(&mut self) -> Result<AstTypeDeclaration, CompileError> {
        let stateful = if self.take(&TokenKind::At) {
            self.expect(&TokenKind::Stateful, "stateful")?;
            true
        } else {
            false
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
            fields,
            variants,
        })
    }

    fn function(&mut self) -> Result<AstFunction, CompileError> {
        let is_task = self.take(&TokenKind::Task);
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
        if self.take(&TokenKind::While) || self.take(&TokenKind::For) {
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
            TokenKind::Ident(name) if self.take(&TokenKind::LParen) => {
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
            }
            TokenKind::Ident(name) => AstExpression::Name(name),
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

pub fn resolve_and_typecheck(ast: AstModule) -> Result<HirModule, CompileError> {
    let mut known_types = [
        "string", "rune", "Array", "Map", "Option", "Result", "Task", "Buffer",
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
    let mut signatures = BTreeMap::new();
    for function in &ast.functions {
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
    for function in ast.functions {
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
        let has_return = check_statements(
            &function.body,
            &mut locals,
            &signatures,
            signature.result.expect("result is required"),
        )?;
        if !has_return {
            return Err(CompileError::MissingReturn);
        }
        functions.push(HirFunction {
            name: function.name,
            signature,
            body: function.body,
            locals,
        });
    }
    Ok(HirModule { functions })
}

fn validate_type(ty: &AstType, known_types: &BTreeSet<String>) -> Result<(), CompileError> {
    if let AstType::Named(name) = ty
        && !known_types.contains(name)
    {
        return Err(CompileError::UnknownType(name.clone()));
    }
    Ok(())
}

fn check_statements(
    statements: &[AstStatement],
    locals: &mut BTreeMap<String, (u16, ValueType)>,
    signatures: &BTreeMap<String, Signature>,
    result: ValueType,
) -> Result<bool, CompileError> {
    let mut has_return = false;
    for statement in statements {
        match statement {
            AstStatement::Bind { name, value } => {
                let ty = expression_type(value, locals, signatures)?;
                let register =
                    u16::try_from(locals.len()).map_err(|_| CompileError::TooManyRegisters)?;
                if locals.insert(name.clone(), (register, ty)).is_some() {
                    return Err(CompileError::DuplicateName(name.clone()));
                }
            }
            AstStatement::Return(expression) => {
                if expression_type(expression, locals, signatures)? != result {
                    return Err(CompileError::TypeMismatch);
                }
                has_return = true;
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
                has_return |= check_statements(then_body, locals, signatures, result)?;
                has_return |= check_statements(else_body, locals, signatures, result)?;
            }
            AstStatement::While { condition, body } => {
                if expression_type(condition, locals, signatures)? != ValueType::Bool {
                    return Err(CompileError::TypeMismatch);
                }
                has_return |= check_statements(body, locals, signatures, result)?;
            }
            AstStatement::Defer(expression) => {
                if contains_await(expression) {
                    return Err(CompileError::SuspendingDefer);
                }
                expression_type(expression, locals, signatures)?;
            }
        }
    }
    Ok(has_return)
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
        AstType::Named(_) => ValueType::Ref,
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
    for function in &hir.functions {
        let mut code = Vec::new();
        let temporary =
            u16::try_from(function.locals.len()).map_err(|_| CompileError::TooManyRegisters)?;
        emit_statements(
            &function.body,
            temporary,
            &function.locals,
            &function_ids,
            &mut code,
        )?;
        let registers =
            u16::try_from(function.locals.len() + 8).map_err(|_| CompileError::TooManyRegisters)?;
        let mut root_bitmap = vec![false; usize::from(registers)];
        for (register, ty) in function.locals.values() {
            root_bitmap[usize::from(*register)] = *ty == ValueType::Ref;
        }
        if function.signature.result == Some(ValueType::Ref) {
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
                        hir.functions[*callee as usize].signature.result == Some(ValueType::Ref),
                    ),
                    _ => continue,
                };
                if is_reference && !root_bitmap[usize::from(destination)] {
                    root_bitmap[usize::from(destination)] = true;
                    changed = true;
                }
            }
        }
        module.function(Function {
            signature: function.signature.clone(),
            registers,
            frame_bytes: u32::from(registers) * 8,
            root_bitmap,
            code,
        });
    }
    Ok(module.finish())
}

fn emit_statements(
    statements: &[AstStatement],
    temporary: u16,
    locals: &BTreeMap<String, (u16, ValueType)>,
    functions: &BTreeMap<String, u32>,
    code: &mut Vec<Instruction>,
) -> Result<(), CompileError> {
    for statement in statements {
        match statement {
            AstStatement::Bind { name, value } => {
                emit_expression(value, locals[name].0, locals, functions, code)?;
            }
            AstStatement::Return(value) => {
                emit_expression(value, temporary, locals, functions, code)?;
                code.push(Instruction::Return { source: temporary });
            }
            AstStatement::Expression(expression) | AstStatement::Defer(expression) => {
                emit_expression(expression, temporary, locals, functions, code)?;
                if matches!(statement, AstStatement::Defer(_)) {
                    code.push(Instruction::Safepoint);
                }
            }
            AstStatement::If {
                condition,
                then_body,
                else_body,
            } => {
                emit_expression(condition, temporary, locals, functions, code)?;
                let branch = code.len();
                code.push(Instruction::JumpIfFalse {
                    condition: temporary,
                    target: 0,
                });
                emit_statements(then_body, temporary, locals, functions, code)?;
                let skip_else = code.len();
                code.push(Instruction::Jump { target: 0 });
                let else_start =
                    u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
                code[branch] = Instruction::JumpIfFalse {
                    condition: temporary,
                    target: else_start,
                };
                emit_statements(else_body, temporary, locals, functions, code)?;
                let end = u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
                code.push(Instruction::Safepoint);
                code[skip_else] = Instruction::Jump { target: end };
            }
            AstStatement::While { condition, body } => {
                let loop_start =
                    u32::try_from(code.len()).map_err(|_| CompileError::TooManyRegisters)?;
                code.push(Instruction::Safepoint);
                emit_expression(condition, temporary, locals, functions, code)?;
                let exit = code.len();
                code.push(Instruction::JumpIfFalse {
                    condition: temporary,
                    target: 0,
                });
                emit_statements(body, temporary, locals, functions, code)?;
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
            emit_expression(lhs, lhs_register, locals, functions, code)?;
            emit_expression(rhs, rhs_register, locals, functions, code)?;
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
            for (index, argument) in arguments.iter().enumerate() {
                emit_expression(
                    argument,
                    u16::try_from(index).map_err(|_| CompileError::TooManyRegisters)?,
                    locals,
                    functions,
                    code,
                )?;
            }
            code.push(Instruction::Call {
                function: *functions
                    .get(function)
                    .ok_or_else(|| CompileError::UnknownName(function.clone()))?,
                args: u16::try_from(arguments.len()).map_err(|_| CompileError::TooManyRegisters)?,
                dst: destination,
            });
        }
        AstExpression::Await(expression) => {
            emit_expression(expression, destination, locals, functions, code)?;
            code.push(Instruction::Yield);
        }
    }
    Ok(())
}

pub fn compile(source: &str) -> Result<VerifiedModule, CompileError> {
    let tokens = lex(source)?;
    let ast = parse(&tokens)?;
    let hir = resolve_and_typecheck(ast)?;
    let module = emit_bytecode(&hir)?;
    verify(module, VerifierLimits::default())
        .map_err(|error| CompileError::Verify(error.to_string()))
}

#[cfg(test)]
mod tests {
    use nexa_runtime::{CheckedInterpreter, GcRef, InterpreterOutcome, RuntimeValue};

    use super::compile;

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
            InterpreterOutcome::Returned(Some(RuntimeValue::I32(42)))
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
            InterpreterOutcome::Returned(Some(RuntimeValue::I32(2)))
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
        let value = RuntimeValue::Ref(GcRef {
            index: 3,
            generation: 4,
        });
        assert!(matches!(
            CheckedInterpreter::run(&reference, 0, &[value], 100).unwrap(),
            InterpreterOutcome::Returned(Some(result)) if result == value
        ));

        let task = compile(
            "fn id(value: i32) -> i32 { return value; }
             task fn update(value: i32) -> i32 { return await id(value); }",
        )
        .unwrap();
        let outcome = CheckedInterpreter::run(&task, 1, &[RuntimeValue::I32(8)], 100).unwrap();
        let InterpreterOutcome::Yielded(continuation) = outcome else {
            panic!("await must suspend");
        };
        assert!(matches!(
            CheckedInterpreter::resume(&task, continuation, 100).unwrap(),
            InterpreterOutcome::Returned(Some(RuntimeValue::I32(8)))
        ));
    }
}
