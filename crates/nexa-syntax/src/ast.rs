//! Complete, owned typed AST views over a lossless [`SyntaxTree`].
//!
//! The AST stores source ranges and semantic token shapes, but deliberately
//! performs no name resolution or type inference. Recovery nodes keep later
//! declarations and statements available after a local syntax error.

use crate::{Keyword, SyntaxLanguage, SyntaxTree, TextRange, TextSize, Token, TokenKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NexaAst {
    pub uses: Vec<UseDeclaration>,
    pub declarations: Vec<Declaration>,
    pub top_level_statements: Vec<Statement>,
    pub top_level_tail: Option<Box<Expression>>,
    pub errors: Vec<AstError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AstError {
    pub kind: AstErrorKind,
    pub range: TextRange,
    pub message: String,
    /// Optional human suggestion rendered as a `= help:` continuation.
    pub fix: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AstErrorKind {
    InvalidSyntax,
    LegacyModuleDeclaration { path: TextRange },
    /// `name!(` looked like a Rust macro invocation; the callee is already explained by the
    /// error message, so downstream name resolution must not re-report it.
    RustMacroInvocation,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Identifier {
    pub text: String,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QualifiedName {
    pub segments: Vec<Identifier>,
    pub range: TextRange,
}

impl QualifiedName {
    #[must_use]
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .join("::")
    }

    #[must_use]
    pub fn last(&self) -> Option<&Identifier> {
        self.segments.last()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum UsePathRootKind {
    Package,
    Self_,
    Super,
    Host,
    Std,
    Dependency,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UsePathRoot {
    pub kind: UsePathRootKind,
    pub name: Identifier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UseDeclaration {
    pub root: UsePathRoot,
    pub segments: Vec<Identifier>,
    pub alias: Option<Identifier>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub visibility: Visibility,
    pub kind: DeclarationKind,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocComment {
    pub text: String,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub name: Identifier,
    pub arguments: Vec<AttributeArgument>,
    pub range: TextRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttributeArgumentKind {
    String,
    Integer,
    Float,
    Rune,
    Bool,
    Identifier,
    Tokens,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttributeArgumentClassification {
    Positional,
    Named,
    Duplicate,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeArgument {
    pub name: Option<Identifier>,
    pub classification: AttributeArgumentClassification,
    pub kind: AttributeArgumentKind,
    pub text: String,
    pub range: TextRange,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Visibility {
    #[default]
    Private,
    Package,
    Public,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeclarationKind {
    Function(FunctionDeclaration),
    Type(TypeDeclaration),
    Const(ConstDeclaration),
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDeclaration {
    pub is_async: bool,
    pub name: Identifier,
    pub parameters: Vec<Parameter>,
    pub result: Option<TypeRef>,
    pub body: Block,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parameter {
    pub name: Identifier,
    pub ty: TypeRef,
    pub range: TextRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeDeclarationKind {
    Struct,
    Enum,
    Class,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeDeclaration {
    pub kind: TypeDeclarationKind,
    pub name: Identifier,
    pub fields: Vec<FieldDeclaration>,
    pub variants: Vec<VariantDeclaration>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDeclaration {
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub mutable: bool,
    pub name: Identifier,
    pub ty: TypeRef,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantDeclaration {
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub name: Identifier,
    pub payload: VariantPayload,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VariantPayload {
    Unit,
    Tuple(Vec<TypeRef>),
    Struct(Vec<VariantFieldDeclaration>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantFieldDeclaration {
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub name: Identifier,
    pub ty: TypeRef,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstDeclaration {
    pub name: Identifier,
    pub ty: TypeRef,
    pub value: Expression,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeRef {
    pub kind: TypeKind,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Named(QualifiedName),
    Generic {
        base: QualifiedName,
        arguments: Vec<TypeRef>,
    },
    Tuple(Vec<TypeRef>),
    Array(Box<TypeRef>),
    Map {
        key: Box<TypeRef>,
        value: Box<TypeRef>,
    },
    Option(Box<TypeRef>),
    Result {
        ok: Box<TypeRef>,
        error: Box<TypeRef>,
    },
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub statements: Vec<Statement>,
    pub tail: Option<Box<Expression>>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub kind: StatementKind,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatementKind {
    Bind {
        mutable: bool,
        name: Identifier,
        ty: Option<TypeRef>,
        value: Expression,
    },
    Assign {
        target: Expression,
        value: Expression,
    },
    Return(Option<Expression>),
    If {
        condition: Expression,
        then_block: Block,
        else_branch: Option<ElseBranch>,
    },
    While {
        condition: Expression,
        body: Block,
    },
    For {
        binding: Identifier,
        start: Expression,
        end: Expression,
        body: Block,
    },
    Break,
    Continue,
    Yield,
    Defer(Expression),
    Expression(Expression),
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ElseBranch {
    Block(Block),
    If(Box<Statement>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expression {
    pub kind: ExpressionKind,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpressionKind {
    Literal(Literal),
    Name(QualifiedName),
    Tuple(Vec<Expression>),
    Array(Vec<Expression>),
    Unary {
        operator: UnaryOperator,
        operand: Box<Expression>,
    },
    Binary {
        left: Box<Expression>,
        operator: BinaryOperator,
        right: Box<Expression>,
    },
    Call {
        callee: Box<Expression>,
        type_arguments: Vec<TypeRef>,
        arguments: Vec<Expression>,
    },
    Member {
        receiver: Box<Expression>,
        member: Identifier,
    },
    Index {
        receiver: Box<Expression>,
        index: Box<Expression>,
    },
    Construct {
        ty: QualifiedName,
        fields: Vec<FieldInitializer>,
        update: Option<Box<Expression>>,
    },
    New {
        ty: TypeRef,
        fields: Vec<FieldInitializer>,
        update: Option<Box<Expression>>,
    },
    Await {
        operand: Box<Expression>,
    },
    Try(Box<Expression>),
    Match {
        value: Box<Expression>,
        arms: Vec<MatchArm>,
    },
    Interpolation(Vec<InterpolationPart>),
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LiteralKind {
    Integer,
    Float,
    Bool,
    Rune,
    String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Literal {
    pub kind: LiteralKind,
    pub raw: String,
    pub cooked: Option<String>,
    pub range: TextRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnaryOperatorKind {
    Positive,
    Negate,
    Not,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnaryOperator {
    pub kind: UnaryOperatorKind,
    pub range: TextRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinaryOperatorKind {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryOperator {
    pub kind: BinaryOperatorKind,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldInitializer {
    pub name: Identifier,
    pub value: Expression,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub value: Expression,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    pub kind: PatternKind,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternKind {
    Wildcard,
    Binding(Identifier),
    Literal(Literal),
    Variant {
        path: QualifiedName,
        payload: Vec<Pattern>,
    },
    Struct {
        path: QualifiedName,
        fields: Vec<PatternField>,
    },
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternField {
    pub name: Identifier,
    pub pattern: Pattern,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterpolationPart {
    Text {
        raw: String,
        cooked: String,
        range: TextRange,
    },
    Expression(Expression),
}

/// Builds a complete typed AST from a lossless Nexa tree.
///
/// Existing lexical/tree errors remain on [`SyntaxTree::errors`]; this result
/// contains additional typed-parser recovery errors.
#[must_use]
pub fn parse_nexa_ast(tree: &SyntaxTree) -> NexaAst {
    if tree.language != SyntaxLanguage::Nexa {
        return NexaAst {
            uses: Vec::new(),
            declarations: Vec::new(),
            top_level_statements: Vec::new(),
            top_level_tail: None,
            errors: vec![AstError {
                kind: AstErrorKind::InvalidSyntax,
                range: tree.root.range,
                message: "a Nexa AST requires Nexa source".into(),
                fix: None,
            }],
        };
    }
    Parser::new(tree).parse()
}

struct Parser<'a> {
    tree: &'a SyntaxTree,
    significant: Vec<usize>,
    cursor: usize,
    errors: Vec<AstError>,
}

impl<'a> Parser<'a> {
    fn new(tree: &'a SyntaxTree) -> Self {
        Self {
            tree,
            significant: tree
                .tokens
                .iter()
                .enumerate()
                .filter_map(|(index, token)| (!token.kind.is_trivia()).then_some(index))
                .collect(),
            cursor: 0,
            errors: Vec::new(),
        }
    }

    fn parse(mut self) -> NexaAst {
        let mut uses = Vec::new();
        let mut declarations = Vec::new();
        let mut top_level_statements = Vec::new();
        let mut top_level_tail = None;
        while !self.at_end() {
            if self.reject_legacy_module_declaration() {
                continue;
            }
            if self.at_keyword(Keyword::Use) {
                uses.push(self.parse_use());
                continue;
            }
            self.reject_legacy_top_level_syntax();
            let before = self.cursor;
            if self.declaration_starts_here() {
                declarations.push(self.parse_declaration());
            } else {
                match self.statement(true) {
                    ParsedStatement::Statement(statement) => top_level_statements.push(statement),
                    ParsedStatement::Tail(expression) => {
                        top_level_tail = Some(Box::new(expression));
                    }
                }
            }
            if self.cursor == before {
                self.bump();
            }
        }
        NexaAst {
            uses,
            declarations,
            top_level_statements,
            top_level_tail,
            errors: self.errors,
        }
    }

    fn reject_legacy_module_declaration(&mut self) -> bool {
        let Some(keyword) = self
            .significant
            .get(self.cursor)
            .map(|index| self.tree.tokens[*index])
        else {
            return false;
        };
        if keyword.kind != TokenKind::Identifier || self.token_text(keyword) != "module" {
            return false;
        }

        let path_start_cursor = self.cursor + 1;
        let Some(first_segment) = self
            .significant
            .get(path_start_cursor)
            .map(|index| self.tree.tokens[*index])
            .filter(|token| token.kind == TokenKind::Identifier)
        else {
            return false;
        };
        let mut cursor = path_start_cursor + 1;
        let mut last_segment = first_segment;
        while self.kind_at(cursor) == Some(TokenKind::Dot)
            && self.kind_at(cursor + 1) == Some(TokenKind::Identifier)
        {
            last_segment = self.token_at_cursor(cursor + 1);
            cursor += 2;
        }
        if self.kind_at(cursor) != Some(TokenKind::Semicolon) {
            return false;
        }

        self.errors.push(AstError {
            kind: AstErrorKind::LegacyModuleDeclaration {
                path: cover(first_segment.range, last_segment.range),
            },
            range: keyword.range,
            message: "legacy module declarations were removed in Nexa v2".into(),
            fix: None,
        });
        self.cursor = cursor + 1;
        true
    }

    fn reject_legacy_top_level_syntax(&mut self) {
        let Some(token) = self
            .significant
            .get(self.cursor)
            .map(|index| self.tree.tokens[*index])
        else {
            return;
        };
        if token.kind != TokenKind::Identifier {
            return;
        }
        let text = self.token_text(token);
        let looks_legacy = match text {
            "module" | "import" => self
                .kind_at(self.cursor + 1)
                .is_some_and(|kind| matches!(kind, TokenKind::Identifier | TokenKind::Keyword(_))),
            "task" | "immediate" | "migration" | "activation" | "cleanup" => {
                self.kind_at(self.cursor + 1) == Some(TokenKind::Keyword(Keyword::Fn))
            }
            "stateful" => self.kind_at(self.cursor + 1) == Some(TokenKind::Keyword(Keyword::Class)),
            _ => false,
        };
        if looks_legacy {
            self.error(
                token.range,
                &format!("legacy `{text}` syntax was removed in Nexa v2"),
            );
        }
    }

    fn parse_use(&mut self) -> UseDeclaration {
        let start = self.bump_range();
        let root_name = self.path_root_identifier();
        let root = UsePathRoot {
            kind: match root_name.text.as_str() {
                "package" => UsePathRootKind::Package,
                "self" => UsePathRootKind::Self_,
                "super" => UsePathRootKind::Super,
                "host" => UsePathRootKind::Host,
                "std" => UsePathRootKind::Std,
                _ => UsePathRootKind::Dependency,
            },
            name: root_name,
        };
        let mut segments = Vec::new();
        while self.take(TokenKind::ColonColon).is_some() {
            segments.push(self.member_identifier());
        }
        if segments.is_empty() {
            self.error(
                root.name.range,
                "use path must include at least one segment",
            );
        }
        let alias = if self.take_keyword(Keyword::As).is_some() {
            Some(self.identifier())
        } else {
            None
        };
        let end = self.expect(TokenKind::Semicolon, "expected `;` after use declaration");
        UseDeclaration {
            root,
            segments,
            alias,
            range: cover(start, end),
        }
    }

    fn declaration_starts_here(&self) -> bool {
        matches!(
            self.current_kind(),
            Some(
                TokenKind::At
                    | TokenKind::Keyword(
                        Keyword::Pub
                            | Keyword::Async
                            | Keyword::Fn
                            | Keyword::Struct
                            | Keyword::Enum
                            | Keyword::Class
                            | Keyword::Const
                    )
            )
        )
    }

    fn parse_declaration(&mut self) -> Declaration {
        let first = self.current_original_index();
        let docs = self.leading_docs(first);
        let start = docs
            .first()
            .map_or_else(|| self.current_range(), |doc| doc.range);
        let attributes = self.attributes();
        let visibility = self.visibility();
        let kind = if self.function_starts_here() {
            DeclarationKind::Function(self.function())
        } else if self.at_keyword(Keyword::Struct) {
            DeclarationKind::Type(self.type_declaration(TypeDeclarationKind::Struct))
        } else if self.at_keyword(Keyword::Enum) {
            DeclarationKind::Type(self.type_declaration(TypeDeclarationKind::Enum))
        } else if self.at_keyword(Keyword::Class) {
            DeclarationKind::Type(self.type_declaration(TypeDeclarationKind::Class))
        } else if self.at_keyword(Keyword::Const) {
            DeclarationKind::Const(self.constant())
        } else {
            self.error_current("expected a top-level declaration");
            self.synchronize_top_level();
            DeclarationKind::Error
        };
        let end = declaration_range(&kind).unwrap_or_else(|| self.previous_range());
        Declaration {
            docs,
            attributes,
            visibility,
            kind,
            range: cover(start, end),
        }
    }

    fn function_starts_here(&self) -> bool {
        matches!(
            self.current_kind(),
            Some(TokenKind::Keyword(Keyword::Fn | Keyword::Async))
        )
    }

    fn attributes(&mut self) -> Vec<Attribute> {
        let mut attributes = Vec::new();
        while self.at(TokenKind::At) {
            let start = self.bump_range();
            let name = self.attribute_name();
            let mut arguments = Vec::new();
            let mut end = name.range;
            if self.take(TokenKind::LParen).is_some() {
                let mut argument_start = self.cursor;
                let mut depth = 0_u32;
                loop {
                    if self.at_end() {
                        if argument_start < self.cursor {
                            arguments.push(self.attribute_argument(argument_start, self.cursor));
                        }
                        self.error_current("unterminated attribute argument list");
                        break;
                    }
                    match self.current_kind() {
                        Some(TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace) => {
                            depth += 1;
                            self.bump();
                        }
                        Some(TokenKind::RParen) if depth == 0 => {
                            if argument_start < self.cursor {
                                arguments
                                    .push(self.attribute_argument(argument_start, self.cursor));
                            }
                            end = self.bump_range();
                            break;
                        }
                        Some(TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace) => {
                            depth = depth.saturating_sub(1);
                            self.bump();
                        }
                        Some(TokenKind::Comma) if depth == 0 => {
                            if argument_start < self.cursor {
                                arguments
                                    .push(self.attribute_argument(argument_start, self.cursor));
                            }
                            self.bump();
                            argument_start = self.cursor;
                        }
                        _ => {
                            self.bump();
                        }
                    }
                }
            }
            Self::classify_attribute_arguments(&name, &mut arguments);
            attributes.push(Attribute {
                name,
                arguments,
                range: cover(start, end),
            });
        }
        attributes
    }

    fn attribute_argument(&self, start: usize, end: usize) -> AttributeArgument {
        let first = self.token_at_cursor(start);
        let last = self.token_at_cursor(end.saturating_sub(1));
        let range = cover(first.range, last.range);
        let named = first.kind == TokenKind::Identifier
            && self.kind_at(start + 1) == Some(TokenKind::Equal)
            && start + 2 < end;
        let value_start = if named { start + 2 } else { start };
        let value_first = self.token_at_cursor(value_start);
        let value_range = cover(value_first.range, last.range);
        let kind = match value_first.kind {
            TokenKind::StringStart => AttributeArgumentKind::String,
            TokenKind::Integer => AttributeArgumentKind::Integer,
            TokenKind::Float => AttributeArgumentKind::Float,
            TokenKind::Rune => AttributeArgumentKind::Rune,
            TokenKind::Keyword(Keyword::True | Keyword::False) => AttributeArgumentKind::Bool,
            TokenKind::Identifier => AttributeArgumentKind::Identifier,
            _ => AttributeArgumentKind::Tokens,
        };
        AttributeArgument {
            name: named.then(|| Identifier {
                text: self.token_text(first).to_owned(),
                range: first.range,
            }),
            classification: if named {
                AttributeArgumentClassification::Named
            } else {
                AttributeArgumentClassification::Positional
            },
            kind,
            text: self.text(value_range).to_owned(),
            range,
        }
    }

    fn classify_attribute_arguments(name: &Identifier, arguments: &mut [AttributeArgument]) {
        for index in 0..arguments.len() {
            let Some(argument_name) = arguments[index].name.as_ref() else {
                continue;
            };
            if arguments[..index]
                .iter()
                .filter_map(|argument| argument.name.as_ref())
                .any(|previous| previous.text == argument_name.text)
            {
                arguments[index].classification = AttributeArgumentClassification::Duplicate;
            } else if name.text != "state" || argument_name.text != "version" {
                arguments[index].classification = AttributeArgumentClassification::Unknown;
            }
        }
    }

    fn visibility(&mut self) -> Visibility {
        if self.take_keyword(Keyword::Pub).is_none() {
            return Visibility::Private;
        }
        if self.take(TokenKind::LParen).is_some() {
            self.expect_keyword(Keyword::Package, "expected `package` in visibility");
            self.expect(TokenKind::RParen, "expected `)` after package visibility");
            Visibility::Package
        } else {
            Visibility::Public
        }
    }

    fn function(&mut self) -> FunctionDeclaration {
        let start = self.current_range();
        let is_async = self.take_keyword(Keyword::Async).is_some();
        self.expect_keyword(Keyword::Fn, "expected `fn`");
        let name = if self.current_kind() == Some(TokenKind::Identifier) {
            self.identifier()
        } else {
            let range = self.current_range();
            self.error(range, "expected function name after `fn`");
            Identifier {
                text: "<missing>".into(),
                range,
            }
        };
        self.require_snake_case(&name, "function");
        self.expect(TokenKind::LParen, "expected `(` after function name");
        let mut parameters = Vec::new();
        let mut parameter_error = false;
        while !self.at_end() && !self.at(TokenKind::RParen) {
            let parameter_start = self.current_range();
            let parameter_name = self.identifier();
            self.require_snake_case(&parameter_name, "parameter");
            if self.take(TokenKind::Colon).is_none() {
                if !parameter_error {
                    self.error(self.current_range(), "expected `:` after parameter name");
                    parameter_error = true;
                }
                // Synchronize to the next separator so a broken parameter list does not cascade
                // into expression/block recovery errors.
                while !self.at_end()
                    && !self.at(TokenKind::Comma)
                    && !self.at(TokenKind::RParen)
                    && !self.at(TokenKind::LBrace)
                {
                    self.bump();
                }
                if self.take(TokenKind::Comma).is_some() {
                    continue;
                }
                break;
            }
            let ty = self.ty();
            parameters.push(Parameter {
                name: parameter_name,
                range: cover(parameter_start, ty.range),
                ty,
            });
            if self.take(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RParen, "expected `)` after parameters");
        let result = self.take(TokenKind::Arrow).map(|_| self.ty());
        let body = self.block();
        FunctionDeclaration {
            is_async,
            name,
            parameters,
            result,
            range: cover(start, body.range),
            body,
        }
    }

    fn type_declaration(&mut self, kind: TypeDeclarationKind) -> TypeDeclaration {
        let start = self.bump_range();
        let name = self.identifier();
        self.require_pascal_case(&name, "type");
        self.expect(TokenKind::LBrace, "expected `{` after type name");
        let mut fields = Vec::new();
        let mut variants = Vec::new();
        while !self.at_end() && !self.at(TokenKind::RBrace) {
            let original = self.current_original_index();
            let docs = self.leading_docs(original);
            let member_start = docs
                .first()
                .map_or_else(|| self.current_range(), |doc| doc.range);
            let attributes = self.attributes();
            let mutable_range = self.take_keyword(Keyword::Mut);
            let mutable = mutable_range.is_some();
            if mutable && kind != TypeDeclarationKind::Class {
                self.error(
                    mutable_range.expect("mutable range exists"),
                    "`mut` is only allowed on class fields",
                );
            }
            let member_name = self.identifier();
            if kind == TypeDeclarationKind::Enum {
                self.require_pascal_case(&member_name, "enum variant");
                let payload = if self.take(TokenKind::LParen).is_some() {
                    let mut elements = Vec::new();
                    while !self.at_end() && !self.at(TokenKind::RParen) {
                        elements.push(self.ty());
                        if self.take(TokenKind::Comma).is_none() {
                            break;
                        }
                    }
                    self.expect(TokenKind::RParen, "expected `)` after enum payload");
                    VariantPayload::Tuple(elements)
                } else if self.take(TokenKind::LBrace).is_some() {
                    VariantPayload::Struct(self.variant_struct_fields())
                } else {
                    VariantPayload::Unit
                };
                let end = if let Some(comma) = self.take(TokenKind::Comma) {
                    comma
                } else if self.at(TokenKind::RBrace) {
                    self.previous_range()
                } else {
                    self.expect(TokenKind::Comma, "expected `,` after enum variant")
                };
                variants.push(VariantDeclaration {
                    docs,
                    attributes,
                    name: member_name,
                    payload,
                    range: cover(member_start, end),
                });
            } else {
                self.require_snake_case(&member_name, "field");
                self.expect(TokenKind::Colon, "expected `:` after field name");
                let ty = self.ty();
                let end = if let Some(comma) = self.take(TokenKind::Comma) {
                    comma
                } else if self.at(TokenKind::RBrace) {
                    ty.range
                } else {
                    self.expect(TokenKind::Comma, "expected `,` after field")
                };
                fields.push(FieldDeclaration {
                    docs,
                    attributes,
                    mutable,
                    name: member_name,
                    ty,
                    range: cover(member_start, end),
                });
            }
        }
        let end = self.expect(TokenKind::RBrace, "expected `}` after type body");
        TypeDeclaration {
            kind,
            name,
            fields,
            variants,
            range: cover(start, end),
        }
    }

    fn variant_struct_fields(&mut self) -> Vec<VariantFieldDeclaration> {
        let mut fields = Vec::new();
        while !self.at_end() && !self.at(TokenKind::RBrace) {
            let original = self.current_original_index();
            let docs = self.leading_docs(original);
            let start = docs
                .first()
                .map_or_else(|| self.current_range(), |doc| doc.range);
            let attributes = self.attributes();
            if let Some(range) = self.take_keyword(Keyword::Mut) {
                self.error(range, "`mut` is not allowed on enum payload fields");
            }
            let name = self.identifier();
            self.require_snake_case(&name, "enum payload field");
            self.expect(TokenKind::Colon, "expected `:` after payload field name");
            let ty = self.ty();
            let end = if let Some(comma) = self.take(TokenKind::Comma) {
                comma
            } else if self.at(TokenKind::RBrace) {
                ty.range
            } else {
                self.expect(TokenKind::Comma, "expected `,` after payload field")
            };
            fields.push(VariantFieldDeclaration {
                docs,
                attributes,
                name,
                ty,
                range: cover(start, end),
            });
        }
        self.expect(TokenKind::RBrace, "expected `}` after enum struct payload");
        fields
    }

    fn constant(&mut self) -> ConstDeclaration {
        let start = self.bump_range();
        let name = self.identifier();
        self.require_screaming_snake_case(&name);
        self.expect(TokenKind::Colon, "expected `:` after constant name");
        let ty = self.ty();
        self.expect(TokenKind::Equal, "expected `=` in constant declaration");
        let value = self.expression(0);
        let end = self.expect(TokenKind::Semicolon, "expected `;` after constant");
        ConstDeclaration {
            name,
            ty,
            value,
            range: cover(start, end),
        }
    }

    fn ty(&mut self) -> TypeRef {
        let start = self.current_range();
        if self.take(TokenKind::LParen).is_some() {
            let mut elements = Vec::new();
            while !self.at_end() && !self.at(TokenKind::RParen) {
                elements.push(self.ty());
                if self.take(TokenKind::Comma).is_none() {
                    break;
                }
            }
            let end = self.expect(TokenKind::RParen, "expected `)` after tuple type");
            return TypeRef {
                kind: TypeKind::Tuple(elements),
                range: cover(start, end),
            };
        }
        if self.take(TokenKind::LBracket).is_some() {
            let element = self.ty();
            let end = self.expect(TokenKind::RBracket, "expected `]` after array type");
            return TypeRef {
                kind: TypeKind::Array(Box::new(element)),
                range: cover(start, end),
            };
        }
        let base = self.qualified_name();
        let mut arguments = Vec::new();
        let end = if self.take(TokenKind::Less).is_some() {
            while !self.at_end() && !self.at(TokenKind::Greater) {
                arguments.push(self.ty());
                if self.take(TokenKind::Comma).is_none() {
                    break;
                }
            }
            self.expect(TokenKind::Greater, "expected `>` after type arguments")
        } else {
            base.range
        };
        let name = base.text();
        let kind = match (name.as_str(), arguments.as_slice()) {
            ("Array", [element]) => TypeKind::Array(Box::new(element.clone())),
            ("Map", [key, value]) => TypeKind::Map {
                key: Box::new(key.clone()),
                value: Box::new(value.clone()),
            },
            ("Option", [inner]) => TypeKind::Option(Box::new(inner.clone())),
            ("Result", [ok, error]) => TypeKind::Result {
                ok: Box::new(ok.clone()),
                error: Box::new(error.clone()),
            },
            (_, []) if matches!(name.as_str(), "Array" | "Map" | "Option" | "Result") => {
                // Keep empty built-in generics as Generic so arity checking reports one
                // root-cause error instead of falling back to an unknown-name lookup.
                TypeKind::Generic { base, arguments }
            }
            (_, []) => TypeKind::Named(base),
            _ => TypeKind::Generic { base, arguments },
        };
        TypeRef {
            kind,
            range: cover(start, end),
        }
    }

    fn block(&mut self) -> Block {
        let start = self.expect(TokenKind::LBrace, "expected `{` to start block");
        let mut statements = Vec::new();
        let mut tail = None;
        while !self.at_end() && !self.at(TokenKind::RBrace) {
            let before = self.cursor;
            let parsed = self.statement(false);
            match parsed {
                ParsedStatement::Statement(statement) => statements.push(statement),
                ParsedStatement::Tail(expression) => {
                    tail = Some(Box::new(expression));
                    if !self.at(TokenKind::RBrace) {
                        self.synchronize_statement();
                    }
                }
            }
            if self.cursor == before {
                self.bump();
            }
        }
        let end = self.expect(TokenKind::RBrace, "expected `}` after block");
        Block {
            statements,
            tail,
            range: cover(start, end),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn statement(&mut self, allow_eof_tail: bool) -> ParsedStatement {
        let start = self.current_range();
        if self.current_identifier_text() == Some("var")
            && self.kind_at(self.cursor + 1) == Some(TokenKind::Identifier)
        {
            let range = self.bump_range();
            self.error(range, "legacy `var` binding was removed; write `let mut`");
            self.synchronize_statement();
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::Error,
                range: cover(start, self.previous_range()),
            });
        }
        if self.take_keyword(Keyword::Return).is_some() {
            let value = if self.at(TokenKind::Semicolon) {
                None
            } else {
                Some(self.expression(0))
            };
            let end = self.expect(TokenKind::Semicolon, "expected `;` after return");
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::Return(value),
                range: cover(start, end),
            });
        }
        if self.take_keyword(Keyword::Let).is_some() {
            let mutable = self.take_keyword(Keyword::Mut).is_some();
            let name = self.identifier();
            self.require_snake_case(&name, "local variable");
            let ty = self.take(TokenKind::Colon).map(|_| self.ty());
            self.expect(TokenKind::Equal, "expected `=` in binding");
            let value = self.expression(0);
            let end = self.expect(TokenKind::Semicolon, "expected `;` after binding");
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::Bind {
                    mutable,
                    name,
                    ty,
                    value,
                },
                range: cover(start, end),
            });
        }
        if self.take_keyword(Keyword::If).is_some() {
            return ParsedStatement::Statement(self.if_statement(start));
        }
        if self.take_keyword(Keyword::While).is_some() {
            let condition = self.expression(0);
            let body = self.block();
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::While { condition, body },
                range: cover(start, self.previous_range()),
            });
        }
        if self.take_keyword(Keyword::For).is_some() {
            let binding = self.identifier();
            self.require_snake_case(&binding, "loop binding");
            self.expect_keyword(Keyword::In, "expected `in` after loop binding");
            let range_start = self.expression(0);
            self.expect(TokenKind::DotDot, "expected `..` in static range");
            let range_end = self.expression(0);
            let body = self.block();
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::For {
                    binding,
                    start: range_start,
                    end: range_end,
                    body,
                },
                range: cover(start, self.previous_range()),
            });
        }
        if self.take_keyword(Keyword::Break).is_some() {
            let end = self.expect(TokenKind::Semicolon, "expected `;` after break");
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::Break,
                range: cover(start, end),
            });
        }
        if self.take_keyword(Keyword::Continue).is_some() {
            let end = self.expect(TokenKind::Semicolon, "expected `;` after continue");
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::Continue,
                range: cover(start, end),
            });
        }
        if self.take_keyword(Keyword::Yield).is_some() {
            let end = self.expect(TokenKind::Semicolon, "expected `;` after yield");
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::Yield,
                range: cover(start, end),
            });
        }
        if self.take_keyword(Keyword::Defer).is_some() {
            let expression = self.expression(0);
            let end = self.expect(TokenKind::Semicolon, "expected `;` after defer");
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::Defer(expression),
                range: cover(start, end),
            });
        }
        let target = self.expression(0);
        if self.take(TokenKind::Equal).is_some() {
            let value = self.expression(0);
            let end = self.expect(TokenKind::Semicolon, "expected `;` after assignment");
            return ParsedStatement::Statement(Statement {
                kind: StatementKind::Assign { target, value },
                range: cover(start, end),
            });
        }
        if let Some(end) = self.take(TokenKind::Semicolon) {
            ParsedStatement::Statement(Statement {
                range: cover(start, end),
                kind: StatementKind::Expression(target),
            })
        } else if self.at(TokenKind::RBrace) || (allow_eof_tail && self.at_end()) {
            ParsedStatement::Tail(target)
        } else {
            self.error_current("expected `;` or `}` after expression");
            self.synchronize_statement();
            ParsedStatement::Statement(Statement {
                kind: StatementKind::Error,
                range: cover(start, self.previous_range()),
            })
        }
    }

    fn if_statement(&mut self, start: TextRange) -> Statement {
        let condition = self.expression(0);
        let then_block = self.block();
        let else_branch = if self.take_keyword(Keyword::Else).is_some() {
            if self.take_keyword(Keyword::If).is_some() {
                let nested_start = self.previous_range();
                Some(ElseBranch::If(Box::new(self.if_statement(nested_start))))
            } else {
                Some(ElseBranch::Block(self.block()))
            }
        } else {
            None
        };
        Statement {
            kind: StatementKind::If {
                condition,
                then_block,
                else_branch,
            },
            range: cover(start, self.previous_range()),
        }
    }

    fn expression(&mut self, minimum_precedence: u8) -> Expression {
        let mut left = self.prefix_expression();
        loop {
            if let Some(postfix) = self.postfix_expression(left.clone()) {
                left = postfix;
                continue;
            }
            let Some((precedence, kind)) = binary_operator(self.current_kind()) else {
                // `name!(` is a Rust macro invocation; Nexa has no macros. Report it once and keep
                // parsing the parenthesized tail as a regular call.
                if self.current_kind() == Some(TokenKind::Bang)
                    && self.kind_at(self.cursor + 1) == Some(TokenKind::LParen)
                {
                    let callee = self.text(left.range);
                    self.errors.push(AstError {
                        kind: AstErrorKind::RustMacroInvocation,
                        range: left.range,
                        message: format!(
                            "`{callee}!` is a Rust macro invocation; Nexa has no macros"
                        ),
                        fix: Some(
                            "use string interpolation or the host `print` function instead".into(),
                        ),
                    });
                    self.bump();
                    continue;
                }
                break;
            };
            if precedence < minimum_precedence {
                break;
            }
            let operator = BinaryOperator {
                kind,
                range: self.bump_range(),
            };
            if self.current_kind() == Some(TokenKind::Equal) {
                // `+=`/`-=`/`*=`/`/=` are lexed as an operator followed by `=`; report them as a
                // single friendly error and keep parsing as if the compound form were explicit.
                let operator_text = self.text(operator.range);
                let equal_range = self.current_range();
                let left_text = self.text(left.range);
                self.error_with_fix(
                    cover(operator.range, equal_range),
                    &format!("`{operator_text}=` is not a Nexa operator"),
                    format!("write `{left_text} = {left_text} {operator_text} 1`"),
                );
                self.bump();
                let right = self.expression(precedence + 1);
                let range = cover(left.range, right.range);
                left = Expression {
                    kind: ExpressionKind::Binary {
                        left: Box::new(left),
                        operator,
                        right: Box::new(right),
                    },
                    range,
                };
                continue;
            }
            let right = self.expression(precedence + 1);
            let range = cover(left.range, right.range);
            left = Expression {
                kind: ExpressionKind::Binary {
                    left: Box::new(left),
                    operator,
                    right: Box::new(right),
                },
                range,
            };
        }
        left
    }

    #[allow(clippy::too_many_lines)]
    fn prefix_expression(&mut self) -> Expression {
        let start = self.current_range();
        match self.current_kind() {
            Some(TokenKind::Plus | TokenKind::Minus | TokenKind::Bang) => {
                let kind = match self.current_kind() {
                    Some(TokenKind::Plus) => UnaryOperatorKind::Positive,
                    Some(TokenKind::Minus) => UnaryOperatorKind::Negate,
                    _ => UnaryOperatorKind::Not,
                };
                let operator = UnaryOperator {
                    kind,
                    range: self.bump_range(),
                };
                let operand = self.expression(7);
                Expression {
                    range: cover(start, operand.range),
                    kind: ExpressionKind::Unary {
                        operator,
                        operand: Box::new(operand),
                    },
                }
            }
            Some(TokenKind::Keyword(Keyword::Await)) => {
                let await_range = self.bump_range();
                self.error(
                    await_range,
                    "prefix `await` is not supported: write `.await` after the operand",
                );
                let expression = self.expression(7);
                Expression {
                    range: cover(start, expression.range),
                    kind: ExpressionKind::Error,
                }
            }
            Some(TokenKind::Keyword(Keyword::Match)) => self.match_expression(),
            Some(TokenKind::Keyword(Keyword::New))
                if self.kind_at(self.cursor + 1) == Some(TokenKind::Dot) =>
            {
                self.keyword_qualified_expression()
            }
            Some(TokenKind::Keyword(Keyword::New)) => self.new_expression(),
            Some(TokenKind::Keyword(Keyword::Package)) => self.keyword_qualified_expression(),
            Some(TokenKind::Integer) => self.literal_expression(LiteralKind::Integer),
            Some(TokenKind::Float) => self.literal_expression(LiteralKind::Float),
            Some(TokenKind::Rune) => self.literal_expression(LiteralKind::Rune),
            Some(TokenKind::Keyword(Keyword::True | Keyword::False)) => {
                self.literal_expression(LiteralKind::Bool)
            }
            Some(TokenKind::StringStart) => self.string_expression(),
            Some(TokenKind::Identifier) => {
                let name = self.qualified_name();
                if self.at(TokenKind::LBrace)
                    && name.last().is_some_and(|last| starts_uppercase(&last.text))
                {
                    let (fields, update) = self.field_initializers();
                    Expression {
                        range: cover(start, self.previous_range()),
                        kind: ExpressionKind::Construct {
                            ty: name,
                            fields,
                            update,
                        },
                    }
                } else {
                    Expression {
                        range: name.range,
                        kind: ExpressionKind::Name(name),
                    }
                }
            }
            Some(TokenKind::LParen) => self.parenthesized_expression(),
            Some(TokenKind::LBracket) => self.array_expression(),
            _ => {
                self.error_current("expected expression");
                let range = self.bump().map_or(start, |token| token.range);
                Expression {
                    kind: ExpressionKind::Error,
                    range,
                }
            }
        }
    }

    fn postfix_expression(&mut self, receiver: Expression) -> Option<Expression> {
        let start = receiver.range;
        if self.take(TokenKind::Dot).is_some() {
            if let Some(end) = self.take_keyword(Keyword::Await) {
                if self.at(TokenKind::LParen) {
                    self.error_current("postfix `.await` does not take parentheses");
                }
                return Some(Expression {
                    kind: ExpressionKind::Await {
                        operand: Box::new(receiver),
                    },
                    range: cover(start, end),
                });
            }
            let member = self.member_identifier();
            let range = cover(start, member.range);
            return Some(Expression {
                kind: ExpressionKind::Member {
                    receiver: Box::new(receiver),
                    member,
                },
                range,
            });
        }
        if self.at(TokenKind::Less) && self.type_arguments_ahead() {
            let type_arguments = self.type_arguments();
            let arguments = self.call_arguments();
            let range = cover(start, self.previous_range());
            return Some(Expression {
                kind: ExpressionKind::Call {
                    callee: Box::new(receiver),
                    type_arguments,
                    arguments,
                },
                range,
            });
        }
        if self.at(TokenKind::LParen) {
            let arguments = self.call_arguments();
            let range = cover(start, self.previous_range());
            return Some(Expression {
                kind: ExpressionKind::Call {
                    callee: Box::new(receiver),
                    type_arguments: Vec::new(),
                    arguments,
                },
                range,
            });
        }
        if self.take(TokenKind::LBracket).is_some() {
            let index = self.expression(0);
            let end = self.expect(TokenKind::RBracket, "expected `]` after index");
            return Some(Expression {
                kind: ExpressionKind::Index {
                    receiver: Box::new(receiver),
                    index: Box::new(index),
                },
                range: cover(start, end),
            });
        }
        if let Some(end) = self.take(TokenKind::Question) {
            return Some(Expression {
                kind: ExpressionKind::Try(Box::new(receiver)),
                range: cover(start, end),
            });
        }
        if self.current_identifier_text() == Some("with")
            && self.kind_at(self.cursor + 1) == Some(TokenKind::LBrace)
        {
            let legacy = self.bump_range();
            self.error(
                legacy,
                "legacy `with` update was removed; use `{ fields, ..value }`",
            );
            let _ = self.field_initializers();
            return Some(Expression {
                kind: ExpressionKind::Error,
                range: cover(start, self.previous_range()),
            });
        }
        None
    }

    fn literal_expression(&mut self, kind: LiteralKind) -> Expression {
        let token = self.bump().expect("literal token exists");
        let raw = self.token_text(token).to_owned();
        Expression {
            kind: ExpressionKind::Literal(Literal {
                kind,
                cooked: literal_cooked(kind, &raw),
                raw,
                range: token.range,
            }),
            range: token.range,
        }
    }

    fn string_expression(&mut self) -> Expression {
        let start = self.bump_range();
        let mut parts = Vec::new();
        let mut has_expression = false;
        while !self.at_end() && !self.at(TokenKind::StringEnd) {
            if self.at(TokenKind::StringText) {
                let token = self.bump().expect("string text exists");
                let raw = self.token_text(token).to_owned();
                parts.push(InterpolationPart::Text {
                    cooked: decode_string_text(&raw),
                    raw,
                    range: token.range,
                });
            } else if self.take(TokenKind::InterpolationStart).is_some() {
                has_expression = true;
                let expression = self.expression(0);
                self.expect(
                    TokenKind::InterpolationEnd,
                    "expected `}` after interpolation",
                );
                parts.push(InterpolationPart::Expression(expression));
            } else {
                self.error_current("unexpected token in string");
                self.bump();
            }
        }
        let end = self.expect(TokenKind::StringEnd, "expected closing string quote");
        let range = cover(start, end);
        if has_expression {
            Expression {
                kind: ExpressionKind::Interpolation(parts),
                range,
            }
        } else {
            let cooked = parts
                .into_iter()
                .filter_map(|part| match part {
                    InterpolationPart::Text { cooked, .. } => Some(cooked),
                    InterpolationPart::Expression(_) => None,
                })
                .collect::<String>();
            Expression {
                kind: ExpressionKind::Literal(Literal {
                    kind: LiteralKind::String,
                    raw: self.text(range).to_owned(),
                    cooked: Some(cooked),
                    range,
                }),
                range,
            }
        }
    }

    fn parenthesized_expression(&mut self) -> Expression {
        let start = self.bump_range();
        if self.at(TokenKind::RParen) {
            let end = self.bump_range();
            return Expression {
                kind: ExpressionKind::Tuple(Vec::new()),
                range: cover(start, end),
            };
        }
        let first = self.expression(0);
        if self.take(TokenKind::Comma).is_none() {
            let end = self.expect(TokenKind::RParen, "expected `)` after expression");
            return Expression {
                range: cover(start, end),
                ..first
            };
        }
        let mut elements = vec![first];
        while !self.at_end() && !self.at(TokenKind::RParen) {
            elements.push(self.expression(0));
            if self.take(TokenKind::Comma).is_none() {
                break;
            }
        }
        let end = self.expect(TokenKind::RParen, "expected `)` after tuple");
        Expression {
            kind: ExpressionKind::Tuple(elements),
            range: cover(start, end),
        }
    }

    fn array_expression(&mut self) -> Expression {
        let start = self.bump_range();
        let mut elements = Vec::new();
        while !self.at_end() && !self.at(TokenKind::RBracket) {
            elements.push(self.expression(0));
            if self.take(TokenKind::Comma).is_none() {
                break;
            }
        }
        let end = self.expect(TokenKind::RBracket, "expected `]` after array");
        Expression {
            kind: ExpressionKind::Array(elements),
            range: cover(start, end),
        }
    }

    fn new_expression(&mut self) -> Expression {
        let start = self.bump_range();
        let ty = self.ty();
        let (fields, update) = if self.at(TokenKind::LBrace) {
            self.field_initializers()
        } else {
            (Vec::new(), None)
        };
        Expression {
            range: cover(start, self.previous_range()),
            kind: ExpressionKind::New { ty, fields, update },
        }
    }

    fn keyword_qualified_expression(&mut self) -> Expression {
        let token = self.bump().expect("qualified keyword exists");
        let first = Identifier {
            text: self.token_text(token).to_owned(),
            range: token.range,
        };
        let mut segments = vec![first];
        while self.take(TokenKind::ColonColon).is_some() {
            segments.push(self.member_identifier());
        }
        let range = cover(
            segments.first().expect("one segment").range,
            segments.last().expect("one segment").range,
        );
        Expression {
            kind: ExpressionKind::Name(QualifiedName { segments, range }),
            range,
        }
    }

    fn match_expression(&mut self) -> Expression {
        let start = self.bump_range();
        let value = self.expression(0);
        self.expect(TokenKind::LBrace, "expected `{` after match value");
        let mut arms = Vec::new();
        while !self.at_end() && !self.at(TokenKind::RBrace) {
            let arm_start = self.current_range();
            let pattern = self.pattern();
            self.expect(TokenKind::FatArrow, "expected `=>` after pattern");
            let arm_value = self.expression(0);
            let end = self.take(TokenKind::Comma).unwrap_or(arm_value.range);
            arms.push(MatchArm {
                pattern,
                value: arm_value,
                range: cover(arm_start, end),
            });
            if !self.at(TokenKind::RBrace) && self.previous_kind() != Some(TokenKind::Comma) {
                self.error_current("expected `,` or `}` after match arm");
                self.synchronize_match_arm();
            }
        }
        let end = self.expect(TokenKind::RBrace, "expected `}` after match arms");
        Expression {
            range: cover(start, end),
            kind: ExpressionKind::Match {
                value: Box::new(value),
                arms,
            },
        }
    }

    #[allow(clippy::too_many_lines)]
    fn pattern(&mut self) -> Pattern {
        let start = self.current_range();
        match self.current_kind() {
            Some(TokenKind::Integer) => self.literal_pattern(LiteralKind::Integer),
            Some(TokenKind::Float) => self.literal_pattern(LiteralKind::Float),
            Some(TokenKind::Rune) => self.literal_pattern(LiteralKind::Rune),
            Some(TokenKind::Keyword(Keyword::True | Keyword::False)) => {
                self.literal_pattern(LiteralKind::Bool)
            }
            Some(TokenKind::StringStart) => {
                let expression = self.string_expression();
                let ExpressionKind::Literal(literal) = expression.kind else {
                    self.error(expression.range, "patterns cannot contain interpolation");
                    return Pattern {
                        kind: PatternKind::Error,
                        range: expression.range,
                    };
                };
                Pattern {
                    range: expression.range,
                    kind: PatternKind::Literal(literal),
                }
            }
            Some(TokenKind::Identifier) => {
                let path = self.qualified_name();
                if path.text() == "_" {
                    return Pattern {
                        kind: PatternKind::Wildcard,
                        range: path.range,
                    };
                }
                if self.take(TokenKind::LBrace).is_some() {
                    let mut fields = Vec::new();
                    while !self.at_end() && !self.at(TokenKind::RBrace) {
                        let field_start = self.current_range();
                        let name = self.identifier();
                        let pattern = if self.take(TokenKind::Colon).is_some() {
                            self.pattern()
                        } else {
                            Pattern {
                                range: name.range,
                                kind: PatternKind::Binding(name.clone()),
                            }
                        };
                        let end = self.take(TokenKind::Comma).unwrap_or(pattern.range);
                        fields.push(PatternField {
                            name,
                            pattern,
                            range: cover(field_start, end),
                        });
                    }
                    let end = self.expect(TokenKind::RBrace, "expected `}` after struct pattern");
                    Pattern {
                        kind: PatternKind::Struct { path, fields },
                        range: cover(start, end),
                    }
                } else if self.take(TokenKind::LParen).is_some() {
                    let mut payload = Vec::new();
                    while !self.at_end() && !self.at(TokenKind::RParen) {
                        payload.push(self.pattern());
                        if self.take(TokenKind::Comma).is_none() {
                            break;
                        }
                    }
                    let end = self.expect(TokenKind::RParen, "expected `)` after variant pattern");
                    Pattern {
                        kind: PatternKind::Variant { path, payload },
                        range: cover(start, end),
                    }
                } else if path.last().is_some_and(|name| starts_uppercase(&name.text)) {
                    Pattern {
                        range: path.range,
                        kind: PatternKind::Variant {
                            path,
                            payload: Vec::new(),
                        },
                    }
                } else if path.segments.len() == 1 {
                    Pattern {
                        range: path.range,
                        kind: PatternKind::Binding(path.segments[0].clone()),
                    }
                } else {
                    Pattern {
                        range: path.range,
                        kind: PatternKind::Variant {
                            path,
                            payload: Vec::new(),
                        },
                    }
                }
            }
            _ => {
                self.error_current("expected pattern");
                let range = self.bump().map_or(start, |token| token.range);
                Pattern {
                    kind: PatternKind::Error,
                    range,
                }
            }
        }
    }

    fn literal_pattern(&mut self, kind: LiteralKind) -> Pattern {
        let expression = self.literal_expression(kind);
        let ExpressionKind::Literal(literal) = expression.kind else {
            unreachable!("literal parser returns a literal");
        };
        Pattern {
            kind: PatternKind::Literal(literal),
            range: expression.range,
        }
    }

    fn field_initializers(&mut self) -> (Vec<FieldInitializer>, Option<Box<Expression>>) {
        self.expect(TokenKind::LBrace, "expected `{` before fields");
        let mut fields = Vec::new();
        let mut update = None;
        while !self.at_end() && !self.at(TokenKind::RBrace) {
            if self.take(TokenKind::DotDot).is_some() {
                let value = self.expression(0);
                update = Some(Box::new(value));
                self.take(TokenKind::Comma);
                if !self.at(TokenKind::RBrace) {
                    self.error_current("struct update must be the final initializer");
                    self.synchronize_field();
                }
                break;
            }
            let start = self.current_range();
            let name = self.identifier();
            self.expect(TokenKind::Colon, "expected `:` after field name");
            let value = self.expression(0);
            let end = self.take(TokenKind::Comma).unwrap_or(value.range);
            fields.push(FieldInitializer {
                name,
                value,
                range: cover(start, end),
            });
            if !self.at(TokenKind::RBrace) && self.previous_kind() != Some(TokenKind::Comma) {
                self.error_current("expected `,` or `}` after field");
                self.synchronize_field();
            }
        }
        self.expect(TokenKind::RBrace, "expected `}` after fields");
        (fields, update)
    }

    fn type_arguments(&mut self) -> Vec<TypeRef> {
        self.expect(TokenKind::Less, "expected `<` before type arguments");
        let mut arguments = Vec::new();
        while !self.at_end() && !self.at(TokenKind::Greater) {
            arguments.push(self.ty());
            if self.take(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::Greater, "expected `>` after type arguments");
        arguments
    }

    fn call_arguments(&mut self) -> Vec<Expression> {
        self.expect(TokenKind::LParen, "expected `(` before call arguments");
        let mut arguments = Vec::new();
        while !self.at_end() && !self.at(TokenKind::RParen) {
            arguments.push(self.expression(0));
            if self.take(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RParen, "expected `)` after call arguments");
        arguments
    }

    fn type_arguments_ahead(&self) -> bool {
        let mut cursor = self.cursor;
        let mut depth = 0_u32;
        while let Some(token) = self
            .significant
            .get(cursor)
            .map(|index| self.tree.tokens[*index])
        {
            match token.kind {
                TokenKind::Less => depth += 1,
                TokenKind::Greater => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return self
                            .significant
                            .get(cursor + 1)
                            .map(|index| self.tree.tokens[*index].kind)
                            == Some(TokenKind::LParen);
                    }
                }
                TokenKind::Semicolon | TokenKind::LBrace | TokenKind::RBrace => return false,
                _ => {}
            }
            cursor += 1;
        }
        false
    }

    fn qualified_name(&mut self) -> QualifiedName {
        let first = self.path_root_identifier();
        let mut segments = vec![first];
        while self.at(TokenKind::ColonColon)
            && self.kind_at(self.cursor + 1).is_some_and(identifier_like)
        {
            self.bump();
            segments.push(self.member_identifier());
        }
        let range = cover(
            segments
                .first()
                .map_or_else(|| self.current_range(), |segment| segment.range),
            segments
                .last()
                .map_or_else(|| self.previous_range(), |segment| segment.range),
        );
        QualifiedName { segments, range }
    }

    fn path_root_identifier(&mut self) -> Identifier {
        let Some(token) = self.bump() else {
            let range = self.current_range();
            self.error(range, "expected path segment");
            return Identifier {
                text: "<missing>".into(),
                range,
            };
        };
        if !matches!(
            token.kind,
            TokenKind::Identifier | TokenKind::Keyword(Keyword::Package)
        ) {
            self.error(token.range, "expected ASCII path segment");
            return Identifier {
                text: "<error>".into(),
                range: token.range,
            };
        }
        Identifier {
            text: self.token_text(token).to_owned(),
            range: token.range,
        }
    }

    fn identifier(&mut self) -> Identifier {
        let Some(token) = self.bump() else {
            let range = self.current_range();
            self.error(range, "expected identifier");
            return Identifier {
                text: "<missing>".into(),
                range,
            };
        };
        if token.kind != TokenKind::Identifier {
            self.error(token.range, "expected ASCII identifier");
            return Identifier {
                text: "<error>".into(),
                range: token.range,
            };
        }
        Identifier {
            text: self.token_text(token).to_owned(),
            range: token.range,
        }
    }

    fn member_identifier(&mut self) -> Identifier {
        let Some(token) = self.bump() else {
            let range = self.current_range();
            self.error(range, "expected member name");
            return Identifier {
                text: "<missing>".into(),
                range,
            };
        };
        if !identifier_like(token.kind) {
            self.error(token.range, "expected member name");
        }
        Identifier {
            text: self.token_text(token).to_owned(),
            range: token.range,
        }
    }

    fn attribute_name(&mut self) -> Identifier {
        let Some(token) = self.bump() else {
            let range = self.current_range();
            self.error(range, "expected attribute name");
            return Identifier {
                text: "<missing>".into(),
                range,
            };
        };
        if !matches!(token.kind, TokenKind::Identifier | TokenKind::Keyword(_)) {
            self.error(token.range, "expected attribute name");
        }
        Identifier {
            text: self.token_text(token).to_owned(),
            range: token.range,
        }
    }

    fn leading_docs(&self, original: usize) -> Vec<DocComment> {
        let mut cursor = original;
        let mut earliest = original;
        while cursor > 0 {
            let previous = cursor - 1;
            let token = self.tree.tokens[previous];
            match token.kind {
                TokenKind::DocComment => {
                    earliest = previous;
                    cursor = previous;
                }
                TokenKind::Whitespace
                    if self
                        .token_text(token)
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count()
                        <= 1 =>
                {
                    cursor = previous;
                }
                _ => break,
            }
        }
        self.tree.tokens[earliest..original]
            .iter()
            .filter(|token| token.kind == TokenKind::DocComment)
            .map(|token| DocComment {
                text: self.token_text(*token).to_owned(),
                range: token.range,
            })
            .collect()
    }

    fn synchronize_top_level(&mut self) {
        let mut braces = 0_u32;
        while !self.at_end() {
            match self.current_kind() {
                Some(TokenKind::LBrace) => braces += 1,
                Some(TokenKind::RBrace) if braces > 0 => braces -= 1,
                Some(TokenKind::Semicolon) if braces == 0 => {
                    self.bump();
                    break;
                }
                Some(kind) if braces == 0 && top_level_start(kind) => break,
                _ => {}
            }
            self.bump();
        }
    }

    fn synchronize_statement(&mut self) {
        while !self.at_end() && !self.at(TokenKind::RBrace) {
            if self.take(TokenKind::Semicolon).is_some() {
                break;
            }
            self.bump();
        }
    }

    fn synchronize_match_arm(&mut self) {
        while !self.at_end() && !self.at(TokenKind::RBrace) {
            if self.take(TokenKind::Comma).is_some() {
                break;
            }
            self.bump();
        }
    }

    fn synchronize_field(&mut self) {
        while !self.at_end() && !self.at(TokenKind::RBrace) {
            if self.take(TokenKind::Comma).is_some() {
                break;
            }
            self.bump();
        }
    }

    fn expect(&mut self, kind: TokenKind, message: &str) -> TextRange {
        if let Some(range) = self.take(kind) {
            range
        } else {
            let range = self.current_range();
            self.error(range, message);
            range
        }
    }

    fn expect_keyword(&mut self, keyword: Keyword, message: &str) -> TextRange {
        if let Some(range) = self.take_keyword(keyword) {
            range
        } else {
            let range = self.current_range();
            self.error(range, message);
            range
        }
    }

    fn take(&mut self, kind: TokenKind) -> Option<TextRange> {
        if self.at(kind) {
            Some(self.bump_range())
        } else {
            None
        }
    }

    fn take_keyword(&mut self, keyword: Keyword) -> Option<TextRange> {
        if self.at_keyword(keyword) {
            Some(self.bump_range())
        } else {
            None
        }
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.current_kind() == Some(kind)
    }

    fn at_keyword(&self, keyword: Keyword) -> bool {
        self.current_kind() == Some(TokenKind::Keyword(keyword))
    }

    fn current_kind(&self) -> Option<TokenKind> {
        self.kind_at(self.cursor)
    }

    fn previous_kind(&self) -> Option<TokenKind> {
        self.cursor
            .checked_sub(1)
            .and_then(|cursor| self.kind_at(cursor))
    }

    fn kind_at(&self, cursor: usize) -> Option<TokenKind> {
        self.significant
            .get(cursor)
            .map(|index| self.tree.tokens[*index].kind)
    }

    fn bump(&mut self) -> Option<Token> {
        let token = self
            .significant
            .get(self.cursor)
            .map(|index| self.tree.tokens[*index])?;
        self.cursor += 1;
        Some(token)
    }

    fn bump_range(&mut self) -> TextRange {
        self.bump()
            .map_or_else(|| self.current_range(), |token| token.range)
    }

    fn current_range(&self) -> TextRange {
        self.significant.get(self.cursor).map_or_else(
            || TextRange::new(self.tree.source.len(), self.tree.source.len()),
            |index| self.tree.tokens[*index].range,
        )
    }

    fn previous_range(&self) -> TextRange {
        self.cursor
            .checked_sub(1)
            .and_then(|cursor| self.significant.get(cursor))
            .map_or_else(
                || TextRange::new(TextSize::ZERO, TextSize::ZERO),
                |index| self.tree.tokens[*index].range,
            )
    }

    fn current_original_index(&self) -> usize {
        self.significant
            .get(self.cursor)
            .copied()
            .unwrap_or(self.tree.tokens.len())
    }

    fn token_at_cursor(&self, cursor: usize) -> Token {
        self.significant.get(cursor).map_or_else(
            || Token {
                kind: TokenKind::Unknown,
                range: self.current_range(),
            },
            |index| self.tree.tokens[*index],
        )
    }

    fn token_text(&self, token: Token) -> &str {
        self.tree.token_text(&token)
    }

    fn current_identifier_text(&self) -> Option<&str> {
        let token = self
            .significant
            .get(self.cursor)
            .map(|index| self.tree.tokens[*index])?;
        (token.kind == TokenKind::Identifier).then(|| self.token_text(token))
    }

    fn require_snake_case(&mut self, identifier: &Identifier, role: &str) {
        if !identifier.text.starts_with('<')
            && !is_snake_case(&identifier.text)
        {
            self.error(
                identifier.range,
                &format!("{role} name must use snake_case"),
            );
        }
    }

    fn require_pascal_case(&mut self, identifier: &Identifier, role: &str) {
        if !identifier.text.starts_with('<')
            && !is_pascal_case(&identifier.text)
        {
            self.error(
                identifier.range,
                &format!("{role} name must use PascalCase"),
            );
        }
    }

    fn require_screaming_snake_case(&mut self, identifier: &Identifier) {
        if !identifier.text.starts_with('<')
            && !is_screaming_snake_case(&identifier.text)
        {
            self.error(identifier.range, "const name must use SCREAMING_SNAKE_CASE");
        }
    }

    fn text(&self, range: TextRange) -> &str {
        self.tree.source.slice(range).unwrap_or("")
    }

    fn error_current(&mut self, message: &str) {
        self.error(self.current_range(), message);
    }

    fn error_with_fix(&mut self, range: TextRange, message: &str, fix: String) {
        if self.errors.iter().any(|error| error.range == range) {
            return;
        }
        self.errors.push(AstError {
            kind: AstErrorKind::InvalidSyntax,
            range,
            message: message.into(),
            fix: Some(fix),
        });
    }

    fn error(&mut self, range: TextRange, message: &str) {
        // Collapse repeated errors at one token position: after the first expectation fails the
        // parser recovers by re-parsing the same token, which would otherwise double-report.
        if self.errors.iter().any(|error| error.range == range) {
            return;
        }
        self.errors.push(AstError {
            kind: AstErrorKind::InvalidSyntax,
            range,
            message: message.into(),
            fix: None,
        });
    }

    fn at_end(&self) -> bool {
        self.cursor >= self.significant.len()
    }
}

enum ParsedStatement {
    Statement(Statement),
    Tail(Expression),
}

fn declaration_range(kind: &DeclarationKind) -> Option<TextRange> {
    match kind {
        DeclarationKind::Function(function) => Some(function.range),
        DeclarationKind::Type(ty) => Some(ty.range),
        DeclarationKind::Const(constant) => Some(constant.range),
        DeclarationKind::Error => None,
    }
}

fn binary_operator(kind: Option<TokenKind>) -> Option<(u8, BinaryOperatorKind)> {
    Some(match kind? {
        TokenKind::PipePipe => (1, BinaryOperatorKind::Or),
        TokenKind::AmpAmp => (2, BinaryOperatorKind::And),
        TokenKind::EqualEqual => (3, BinaryOperatorKind::Equal),
        TokenKind::BangEqual => (3, BinaryOperatorKind::NotEqual),
        TokenKind::Less => (4, BinaryOperatorKind::Less),
        TokenKind::LessEqual => (4, BinaryOperatorKind::LessEqual),
        TokenKind::Greater => (4, BinaryOperatorKind::Greater),
        TokenKind::GreaterEqual => (4, BinaryOperatorKind::GreaterEqual),
        TokenKind::Plus => (5, BinaryOperatorKind::Add),
        TokenKind::Minus => (5, BinaryOperatorKind::Subtract),
        TokenKind::Star => (6, BinaryOperatorKind::Multiply),
        TokenKind::Slash => (6, BinaryOperatorKind::Divide),
        TokenKind::Percent => (6, BinaryOperatorKind::Remainder),
        _ => return None,
    })
}

fn identifier_like(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier | TokenKind::Keyword(Keyword::New)
    )
}

fn top_level_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::At
            | TokenKind::Keyword(
                Keyword::Pub
                    | Keyword::Async
                    | Keyword::Fn
                    | Keyword::Struct
                    | Keyword::Enum
                    | Keyword::Class
                    | Keyword::Const
                    | Keyword::Use
            )
    )
}

fn starts_uppercase(text: &str) -> bool {
    text.chars().next().is_some_and(char::is_uppercase)
}

fn is_snake_case(text: &str) -> bool {
    let mut characters = text.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_lowercase())
        && characters.all(|character| {
            character == '_' || character.is_ascii_lowercase() || character.is_ascii_digit()
        })
        && !text.ends_with('_')
        && !text.contains("__")
}

fn is_pascal_case(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn is_screaming_snake_case(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && text.chars().all(|character| {
            character == '_' || character.is_ascii_uppercase() || character.is_ascii_digit()
        })
        && !text.ends_with('_')
        && !text.contains("__")
}

fn literal_cooked(kind: LiteralKind, raw: &str) -> Option<String> {
    match kind {
        LiteralKind::Rune => decode_quoted(raw, '\''),
        LiteralKind::String => decode_quoted(raw, '"'),
        LiteralKind::Integer | LiteralKind::Float | LiteralKind::Bool => Some(raw.into()),
    }
}

fn decode_quoted(raw: &str, quote: char) -> Option<String> {
    let inner = raw.strip_prefix(quote)?.strip_suffix(quote)?;
    Some(decode_string_text(inner))
}

fn decode_string_text(raw: &str) -> String {
    let mut output = String::new();
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let Some(escape) = characters.next() else {
            output.push('\\');
            break;
        };
        match escape {
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            '\\' => output.push('\\'),
            '"' => output.push('"'),
            '\'' => output.push('\''),
            '$' if characters.peek() == Some(&'{') => {
                output.push('$');
                output.push(characters.next().expect("peeked interpolation brace"));
            }
            _ => {
                output.push('\\');
                output.push(escape);
            }
        }
    }
    output
}

fn cover(start: TextRange, end: TextRange) -> TextRange {
    TextRange::new(start.start, end.end)
}
