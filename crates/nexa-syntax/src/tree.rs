use std::ops::Range;

use crate::{
    Keyword, Lexed, SourceText, SourceTooLarge, TextRange, TextSize, Token, TokenKind, lex_nexa,
    lex_nidl,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyntaxLanguage {
    Nexa,
    Nidl,
}

/// Whether a REPL submission is structurally complete enough to analyze.
///
/// This classification deliberately answers only the continuation question. A complete
/// submission may still contain ordinary syntax errors, which the normal parser and analyzer
/// report with their precise source ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CellCompleteness {
    Complete,
    Incomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyntaxErrorKind {
    UnexpectedCharacter,
    UnterminatedBlockComment,
    UnterminatedString,
    UnterminatedInterpolation,
    UnterminatedRune,
    InvalidEscape,
    InvalidRune,
    UnmatchedDelimiter,
    UnclosedDelimiter,
    ExpectedIdentifier,
    ExpectedSemicolon,
    MissingContract,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxError {
    pub kind: SyntaxErrorKind,
    pub range: TextRange,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Root,
    UseDeclaration,
    FunctionDeclaration,
    StructDeclaration,
    EnumDeclaration,
    ClassDeclaration,
    ConstDeclaration,
    TopLevelStatement,
    ContractDeclaration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxNode {
    pub kind: NodeKind,
    pub range: TextRange,
    token_range: Range<usize>,
    pub children: Vec<SyntaxNode>,
}

impl SyntaxNode {
    #[must_use]
    pub fn token_range(&self) -> Range<usize> {
        self.token_range.clone()
    }
}

/// An immutable lossless syntax tree with recoverable errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxTree {
    pub language: SyntaxLanguage,
    pub source: SourceText,
    pub tokens: Vec<Token>,
    pub root: SyntaxNode,
    pub errors: Vec<SyntaxError>,
}

impl SyntaxTree {
    #[must_use]
    pub fn reconstructed(&self) -> String {
        let mut reconstructed = String::with_capacity(self.source.as_str().len());
        for token in &self.tokens {
            reconstructed.push_str(
                self.source
                    .slice(token.range)
                    .expect("syntax token range is valid"),
            );
        }
        reconstructed
    }

    #[must_use]
    pub const fn ast(&self) -> AstRoot<'_> {
        AstRoot { tree: self }
    }

    #[must_use]
    pub const fn nidl(&self) -> NidlRoot<'_> {
        NidlRoot { tree: self }
    }

    #[must_use]
    pub fn token_text(&self, token: &Token) -> &str {
        self.source
            .slice(token.range)
            .expect("syntax token range is valid")
    }

    fn node_tokens(&self, node: &SyntaxNode) -> &[Token] {
        &self.tokens[node.token_range.clone()]
    }
}

pub fn parse_nexa(source: &str) -> Result<SyntaxTree, SourceTooLarge> {
    let lexed = lex_nexa(source)?;
    Ok(parse_lexed(lexed, SyntaxLanguage::Nexa))
}

pub fn parse_nidl(source: &str) -> Result<SyntaxTree, SourceTooLarge> {
    let lexed = lex_nidl(source)?;
    Ok(parse_lexed(lexed, SyntaxLanguage::Nidl))
}

/// Classifies one Nexa REPL submission using the canonical lexer and delimiter validator.
///
/// Unterminated lexical constructs and genuinely unclosed delimiters request another input line.
/// An unmatched closing delimiter makes the cell complete-but-invalid so the analyzer can report
/// it immediately instead of leaving the prompt stuck in continuation mode.
pub fn classify_cell_completeness(source: &str) -> Result<CellCompleteness, SourceTooLarge> {
    let tree = parse_nexa(source)?;
    if tree
        .errors
        .iter()
        .any(|error| error.kind == SyntaxErrorKind::UnmatchedDelimiter)
    {
        return Ok(CellCompleteness::Complete);
    }
    let incomplete = tree.errors.iter().any(|error| {
        matches!(
            error.kind,
            SyntaxErrorKind::UnterminatedBlockComment
                | SyntaxErrorKind::UnterminatedString
                | SyntaxErrorKind::UnterminatedInterpolation
                | SyntaxErrorKind::UnterminatedRune
                | SyntaxErrorKind::UnclosedDelimiter
        )
    });
    Ok(if incomplete {
        CellCompleteness::Incomplete
    } else {
        CellCompleteness::Complete
    })
}

fn parse_lexed(lexed: Lexed, language: SyntaxLanguage) -> SyntaxTree {
    let Lexed {
        source,
        tokens,
        errors,
    } = lexed;
    let (root, errors) = {
        let mut parser = Parser {
            source: &source,
            tokens: &tokens,
            significant: tokens
                .iter()
                .enumerate()
                .filter_map(|(index, token)| (!token.kind.is_trivia()).then_some(index))
                .collect(),
            errors,
        };
        parser.validate_delimiters();
        let root = match language {
            SyntaxLanguage::Nexa => parser.nexa_root(),
            SyntaxLanguage::Nidl => parser.nidl_root(),
        };
        (root, parser.errors)
    };
    SyntaxTree {
        language,
        source,
        tokens,
        root,
        errors,
    }
}

struct Parser<'a> {
    source: &'a SourceText,
    tokens: &'a [Token],
    significant: Vec<usize>,
    errors: Vec<SyntaxError>,
}

impl Parser<'_> {
    fn validate_delimiters(&mut self) {
        let mut stack: Vec<(TokenKind, TextRange)> = Vec::new();
        for &index in &self.significant {
            let token = self.tokens[index];
            match token.kind {
                TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => {
                    stack.push((token.kind, token.range));
                }
                TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => {
                    let expected = matching_open(token.kind);
                    if stack.last().is_some_and(|(kind, _)| *kind == expected) {
                        stack.pop();
                    } else {
                        self.errors.push(SyntaxError {
                            kind: SyntaxErrorKind::UnmatchedDelimiter,
                            range: token.range,
                            message: format!("unmatched closing delimiter `{}`", self.text(index)),
                        });
                    }
                }
                _ => {}
            }
        }
        for (_, range) in stack {
            self.errors.push(SyntaxError {
                kind: SyntaxErrorKind::UnclosedDelimiter,
                range,
                message: "unclosed delimiter".into(),
            });
        }
    }

    fn nexa_root(&mut self) -> SyntaxNode {
        let mut children = Vec::new();
        let mut cursor = 0_usize;
        while cursor < self.significant.len() {
            let start_cursor = cursor;
            let (after_prefix, declaration_keyword) = self.declaration_prefix(cursor);
            cursor = after_prefix;
            let Some(&token_index) = self.significant.get(cursor) else {
                break;
            };
            let kind = match self.tokens[token_index].kind {
                TokenKind::Keyword(Keyword::Use) => NodeKind::UseDeclaration,
                TokenKind::Keyword(Keyword::Fn) => NodeKind::FunctionDeclaration,
                TokenKind::Keyword(Keyword::Struct) => NodeKind::StructDeclaration,
                TokenKind::Keyword(Keyword::Enum) => NodeKind::EnumDeclaration,
                TokenKind::Keyword(Keyword::Class) => NodeKind::ClassDeclaration,
                TokenKind::Keyword(Keyword::Const) => NodeKind::ConstDeclaration,
                _ if declaration_keyword.is_some() => declaration_keyword.expect("checked"),
                _ => NodeKind::TopLevelStatement,
            };
            let end_cursor = self.item_end(cursor, kind);
            let end_cursor = end_cursor.max(cursor + 1).min(self.significant.len());
            let first = self.attached_doc_start(self.significant[start_cursor]);
            let last = self.significant[end_cursor - 1];
            if matches!(kind, NodeKind::UseDeclaration | NodeKind::ConstDeclaration)
                && self.tokens[last].kind != TokenKind::Semicolon
            {
                self.errors.push(SyntaxError {
                    kind: SyntaxErrorKind::ExpectedSemicolon,
                    range: self.tokens[last].range,
                    message: "expected `;` after declaration".into(),
                });
            }
            if kind == NodeKind::UseDeclaration {
                self.validate_use_declaration(cursor, end_cursor);
            }
            children.push(self.node(kind, first, last));
            cursor = end_cursor;
        }
        self.root(children)
    }

    fn validate_use_declaration(&mut self, cursor: usize, end: usize) {
        let mut current = cursor + 1;
        let mut expects_identifier = true;
        let mut saw_identifier = false;
        while current < end {
            let Some(token_kind) = self.kind_at(current) else {
                break;
            };
            match token_kind {
                TokenKind::Identifier | TokenKind::Keyword(Keyword::Package)
                    if expects_identifier =>
                {
                    saw_identifier = true;
                    expects_identifier = false;
                }
                TokenKind::ColonColon if !expects_identifier => {
                    expects_identifier = true;
                }
                TokenKind::Keyword(Keyword::As) if saw_identifier && !expects_identifier => {
                    let alias = self.kind_at(current + 1);
                    if alias != Some(TokenKind::Identifier) {
                        self.path_error(current, "expected a use alias after `as`");
                    } else if self.kind_at(current + 2) != Some(TokenKind::Semicolon) {
                        self.path_error(current + 2, "expected `;` after use alias");
                    }
                    return;
                }
                TokenKind::Semicolon if saw_identifier && !expects_identifier => return,
                _ => {
                    self.path_error(
                        current,
                        if expects_identifier {
                            "expected an ASCII use path segment"
                        } else {
                            "expected `::`, `as`, or `;` after use path segment"
                        },
                    );
                    return;
                }
            }
            current += 1;
        }
        let error_cursor = current
            .min(end.saturating_sub(1))
            .min(self.significant.len().saturating_sub(1));
        self.path_error(error_cursor, "incomplete use declaration");
    }

    fn path_error(&mut self, cursor: usize, message: &str) {
        let Some(&token) = self.significant.get(cursor) else {
            return;
        };
        self.errors.push(SyntaxError {
            kind: SyntaxErrorKind::ExpectedIdentifier,
            range: self.tokens[token].range,
            message: message.into(),
        });
    }

    fn nidl_root(&mut self) -> SyntaxNode {
        let Some(contract_cursor) = self
            .significant
            .iter()
            .position(|index| self.tokens[*index].kind == TokenKind::Keyword(Keyword::Contract))
        else {
            self.errors.push(SyntaxError {
                kind: SyntaxErrorKind::MissingContract,
                range: TextRange::new(TextSize::ZERO, TextSize::ZERO),
                message: "expected an NIDL `contract` declaration".into(),
            });
            return self.root(Vec::new());
        };
        let first = self.significant[contract_cursor];
        let last_cursor = self.item_end(contract_cursor, NodeKind::ContractDeclaration);
        let last = self.significant[last_cursor.saturating_sub(1).max(contract_cursor)];
        self.root(vec![self.node(NodeKind::ContractDeclaration, first, last)])
    }

    fn declaration_prefix(&self, mut cursor: usize) -> (usize, Option<NodeKind>) {
        let mut declaration_kind = None;
        while let Some(&index) = self.significant.get(cursor) {
            match self.tokens[index].kind {
                TokenKind::At => {
                    cursor = self.skip_attribute(cursor);
                }
                TokenKind::Keyword(Keyword::Pub) => {
                    cursor += 1;
                    if self.kind_at(cursor) == Some(TokenKind::LParen)
                        && self.kind_at(cursor + 1) == Some(TokenKind::Keyword(Keyword::Package))
                        && self.kind_at(cursor + 2) == Some(TokenKind::RParen)
                    {
                        cursor += 3;
                    }
                }
                TokenKind::Keyword(Keyword::Async) => {
                    declaration_kind = Some(NodeKind::FunctionDeclaration);
                    cursor += 1;
                }
                _ => break,
            }
        }
        (cursor, declaration_kind)
    }

    fn skip_attribute(&self, cursor: usize) -> usize {
        let mut next = cursor + 1;
        if self
            .kind_at(next)
            .is_some_and(|kind| matches!(kind, TokenKind::Identifier | TokenKind::Keyword(_)))
        {
            next += 1;
        }
        if self.kind_at(next) != Some(TokenKind::LParen) {
            return next;
        }
        let mut depth = 0_u32;
        while next < self.significant.len() {
            match self.kind_at(next) {
                Some(TokenKind::LParen) => depth += 1,
                Some(TokenKind::RParen) => {
                    depth = depth.saturating_sub(1);
                    next += 1;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                _ => {}
            }
            next += 1;
        }
        next
    }

    fn item_end(&self, cursor: usize, kind: NodeKind) -> usize {
        let mut current = cursor;
        let mut parens = 0_u32;
        let mut brackets = 0_u32;
        let mut braces = 0_u32;
        let mut saw_body = false;
        while current < self.significant.len() {
            match self.kind_at(current) {
                Some(TokenKind::LParen) => parens += 1,
                Some(TokenKind::RParen) => parens = parens.saturating_sub(1),
                Some(TokenKind::LBracket) => brackets += 1,
                Some(TokenKind::RBracket) => brackets = brackets.saturating_sub(1),
                Some(TokenKind::LBrace) => {
                    braces += 1;
                    saw_body = true;
                }
                Some(TokenKind::RBrace) => {
                    if braces > 0 {
                        braces -= 1;
                        if braces == 0 && saw_body && kind != NodeKind::ConstDeclaration {
                            return current + 1;
                        }
                    } else {
                        return current;
                    }
                }
                Some(TokenKind::Semicolon) if parens == 0 && brackets == 0 && braces == 0 => {
                    return current + 1;
                }
                _ => {}
            }
            current += 1;
            if kind == NodeKind::TopLevelStatement
                && current > cursor
                && self.kind_at(current).is_some_and(is_top_level_start)
            {
                return current;
            }
        }
        current
    }

    fn root(&self, children: Vec<SyntaxNode>) -> SyntaxNode {
        SyntaxNode {
            kind: NodeKind::Root,
            range: TextRange::new(TextSize::ZERO, self.source.len()),
            token_range: 0..self.tokens.len(),
            children,
        }
    }

    fn attached_doc_start(&self, first: usize) -> usize {
        let mut cursor = first;
        let mut earliest_doc = None;
        while cursor > 0 {
            let previous = cursor - 1;
            let token = self.tokens[previous];
            match token.kind {
                TokenKind::DocComment => {
                    earliest_doc = Some(previous);
                    cursor = previous;
                }
                TokenKind::Whitespace
                    if self
                        .text(previous)
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
        earliest_doc.unwrap_or(first)
    }

    fn node(&self, kind: NodeKind, first: usize, last: usize) -> SyntaxNode {
        SyntaxNode {
            kind,
            range: TextRange::new(self.tokens[first].range.start, self.tokens[last].range.end),
            token_range: first..last + 1,
            children: Vec::new(),
        }
    }

    fn kind_at(&self, cursor: usize) -> Option<TokenKind> {
        self.significant
            .get(cursor)
            .map(|index| self.tokens[*index].kind)
    }

    fn text(&self, token: usize) -> &str {
        self.source
            .slice(self.tokens[token].range)
            .expect("parser token range is valid")
    }
}

fn matching_open(close: TokenKind) -> TokenKind {
    match close {
        TokenKind::RParen => TokenKind::LParen,
        TokenKind::RBrace => TokenKind::LBrace,
        TokenKind::RBracket => TokenKind::LBracket,
        _ => close,
    }
}

fn is_top_level_start(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::At
            | TokenKind::Keyword(
                Keyword::Use
                    | Keyword::Pub
                    | Keyword::Async
                    | Keyword::Fn
                    | Keyword::Struct
                    | Keyword::Enum
                    | Keyword::Class
                    | Keyword::Const
            )
    )
}

#[derive(Clone, Copy, Debug)]
pub struct AstRoot<'a> {
    tree: &'a SyntaxTree,
}

impl<'a> AstRoot<'a> {
    pub fn uses(self) -> impl Iterator<Item = UseDeclaration<'a>> {
        self.tree
            .root
            .children
            .iter()
            .filter(|node| node.kind == NodeKind::UseDeclaration)
            .map(|node| UseDeclaration {
                tree: self.tree,
                node,
            })
    }

    pub fn declarations(self) -> impl Iterator<Item = Declaration<'a>> {
        self.tree
            .root
            .children
            .iter()
            .filter(|node| {
                matches!(
                    node.kind,
                    NodeKind::FunctionDeclaration
                        | NodeKind::StructDeclaration
                        | NodeKind::EnumDeclaration
                        | NodeKind::ClassDeclaration
                        | NodeKind::ConstDeclaration
                )
            })
            .map(|node| Declaration {
                tree: self.tree,
                node,
            })
    }

    pub fn top_level_statements(self) -> impl Iterator<Item = &'a SyntaxNode> {
        self.tree
            .root
            .children
            .iter()
            .filter(|node| node.kind == NodeKind::TopLevelStatement)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct UseDeclaration<'a> {
    tree: &'a SyntaxTree,
    node: &'a SyntaxNode,
}

impl<'a> UseDeclaration<'a> {
    #[must_use]
    pub fn path(&self) -> Option<String> {
        qualified_path_after(self.tree, self.node, Keyword::Use)
    }

    #[must_use]
    pub fn alias(&self) -> Option<&'a str> {
        let tokens = significant_node_tokens(self.tree, self.node);
        let alias_position = tokens
            .iter()
            .position(|token| token.kind == TokenKind::Keyword(Keyword::As))?;
        let alias = tokens.get(alias_position + 1)?;
        (alias.kind == TokenKind::Identifier).then(|| self.tree.token_text(alias))
    }

    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.node.range
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Visibility {
    Private,
    Package,
    Public,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeclarationKind {
    Function,
    Struct,
    Enum,
    Class,
    Const,
}

#[derive(Clone, Copy, Debug)]
pub struct Declaration<'a> {
    tree: &'a SyntaxTree,
    node: &'a SyntaxNode,
}

impl<'a> Declaration<'a> {
    fn declaration_keyword(&self) -> Keyword {
        match self.kind() {
            DeclarationKind::Function => Keyword::Fn,
            DeclarationKind::Struct => Keyword::Struct,
            DeclarationKind::Enum => Keyword::Enum,
            DeclarationKind::Class => Keyword::Class,
            DeclarationKind::Const => Keyword::Const,
        }
    }

    #[must_use]
    pub fn kind(&self) -> DeclarationKind {
        match self.node.kind {
            NodeKind::FunctionDeclaration => DeclarationKind::Function,
            NodeKind::StructDeclaration => DeclarationKind::Struct,
            NodeKind::EnumDeclaration => DeclarationKind::Enum,
            NodeKind::ClassDeclaration => DeclarationKind::Class,
            NodeKind::ConstDeclaration => DeclarationKind::Const,
            _ => unreachable!("Declaration wraps declaration nodes"),
        }
    }

    #[must_use]
    pub fn visibility(&self) -> Visibility {
        let tokens = significant_node_tokens(self.tree, self.node);
        let declaration = tokens
            .iter()
            .position(|token| token.kind == TokenKind::Keyword(self.declaration_keyword()))
            .unwrap_or(tokens.len());
        let Some(public) = tokens[..declaration]
            .iter()
            .position(|token| token.kind == TokenKind::Keyword(Keyword::Pub))
        else {
            return Visibility::Private;
        };
        if tokens.get(public + 1).map(|token| token.kind) == Some(TokenKind::LParen)
            && tokens.get(public + 2).map(|token| token.kind)
                == Some(TokenKind::Keyword(Keyword::Package))
            && tokens.get(public + 3).map(|token| token.kind) == Some(TokenKind::RParen)
        {
            Visibility::Package
        } else {
            Visibility::Public
        }
    }

    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let tokens = significant_node_tokens(self.tree, self.node);
        let keyword_position = tokens
            .iter()
            .position(|token| token.kind == TokenKind::Keyword(self.declaration_keyword()))?;
        let name = tokens.get(keyword_position + 1)?;
        (name.kind == TokenKind::Identifier).then(|| self.tree.token_text(name))
    }

    #[must_use]
    pub fn attributes(&self) -> Vec<&'a str> {
        let tokens = significant_node_tokens(self.tree, self.node);
        let declaration = tokens
            .iter()
            .position(|token| token.kind == TokenKind::Keyword(self.declaration_keyword()))
            .unwrap_or(tokens.len());
        tokens[..declaration]
            .windows(2)
            .filter(|pair| {
                pair[0].kind == TokenKind::At
                    && matches!(pair[1].kind, TokenKind::Identifier | TokenKind::Keyword(_))
            })
            .map(|pair| self.tree.token_text(pair[1]))
            .collect()
    }

    /// Raw `///` comment tokens attached to this declaration, in source order.
    #[must_use]
    pub fn doc_comments(&self) -> Vec<&'a str> {
        let tokens = self.tree.node_tokens(self.node);
        let declaration = tokens
            .iter()
            .position(|token| token.kind == TokenKind::Keyword(self.declaration_keyword()))
            .unwrap_or(tokens.len());
        tokens[..declaration]
            .iter()
            .filter(|token| token.kind == TokenKind::DocComment)
            .map(|token| self.tree.token_text(token))
            .collect()
    }

    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.node.range
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NidlRoot<'a> {
    tree: &'a SyntaxTree,
}

impl<'a> NidlRoot<'a> {
    #[must_use]
    pub fn contract_name(&self) -> Option<&'a str> {
        let node = self
            .tree
            .root
            .children
            .iter()
            .find(|node| node.kind == NodeKind::ContractDeclaration)?;
        let tokens = significant_node_tokens(self.tree, node);
        let contract = tokens
            .iter()
            .position(|token| token.kind == TokenKind::Keyword(Keyword::Contract))?;
        let name = tokens.get(contract + 1)?;
        (name.kind == TokenKind::Identifier).then(|| self.tree.token_text(name))
    }
}

fn significant_node_tokens<'a>(tree: &'a SyntaxTree, node: &'a SyntaxNode) -> Vec<&'a Token> {
    tree.node_tokens(node)
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect()
}

fn qualified_path_after(tree: &SyntaxTree, node: &SyntaxNode, keyword: Keyword) -> Option<String> {
    let tokens = significant_node_tokens(tree, node);
    let mut cursor = tokens
        .iter()
        .position(|token| token.kind == TokenKind::Keyword(keyword))?
        + 1;
    let mut result = String::new();
    let mut expects_identifier = true;
    loop {
        let token = tokens.get(cursor)?;
        match token.kind {
            TokenKind::Identifier | TokenKind::Keyword(Keyword::Package) if expects_identifier => {
                result.push_str(tree.token_text(token));
                expects_identifier = false;
            }
            TokenKind::ColonColon if !expects_identifier => {
                result.push_str("::");
                expects_identifier = true;
            }
            TokenKind::Keyword(Keyword::As) | TokenKind::Semicolon if !expects_identifier => {
                return Some(result);
            }
            _ => return None,
        }
        cursor += 1;
    }
}
