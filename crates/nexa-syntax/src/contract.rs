//! Owned, structured NIDL v2 AST built directly from the lossless syntax tree.
//!
//! This is the only NIDL grammar parser. `nexa-idl` consumes these nodes and
//! performs semantic validation; it must not tokenize or parse source again.

use crate::{SourceText, SyntaxLanguage, SyntaxTree, TextRange, Token, TokenKind, ast::Identifier};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractAst {
    pub source: SourceText,
    pub contract: ContractHeader,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractAstError {
    pub range: TextRange,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractHeader {
    pub name: Identifier,
    pub docs: Vec<ContractDocComment>,
    pub attributes: Vec<ContractAttribute>,
    pub items: Vec<ContractItem>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractItem {
    Handle(ContractHandleDecl),
    Struct(ContractStructDecl),
    Enum(ContractEnumDecl),
    FunctionBlock(ContractFunctionBlock),
}

impl ContractItem {
    #[must_use]
    pub const fn range(&self) -> TextRange {
        match self {
            Self::Handle(declaration) => declaration.range,
            Self::Struct(declaration) => declaration.range,
            Self::Enum(declaration) => declaration.range,
            Self::FunctionBlock(block) => block.range,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContractFunctionBlockKind {
    Host,
    Nexa,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractFunctionBlock {
    pub kind: ContractFunctionBlockKind,
    pub docs: Vec<ContractDocComment>,
    pub attributes: Vec<ContractAttribute>,
    pub functions: Vec<ContractFunction>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractHandleDecl {
    pub name: Identifier,
    pub docs: Vec<ContractDocComment>,
    pub attributes: Vec<ContractAttribute>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractStructDecl {
    pub name: Identifier,
    pub docs: Vec<ContractDocComment>,
    pub attributes: Vec<ContractAttribute>,
    pub fields: Vec<ContractField>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractField {
    pub name: Identifier,
    pub docs: Vec<ContractDocComment>,
    pub attributes: Vec<ContractAttribute>,
    pub ty: ContractTypeRef,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractEnumDecl {
    pub name: Identifier,
    pub docs: Vec<ContractDocComment>,
    pub attributes: Vec<ContractAttribute>,
    pub variants: Vec<ContractVariant>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractVariant {
    pub name: Identifier,
    pub docs: Vec<ContractDocComment>,
    pub attributes: Vec<ContractAttribute>,
    pub payload: Option<ContractTypeRef>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractFunction {
    pub name: Identifier,
    pub docs: Vec<ContractDocComment>,
    pub attributes: Vec<ContractAttribute>,
    pub is_async: bool,
    pub parameters: Vec<ContractParameter>,
    pub result: Option<ContractTypeRef>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractParameter {
    pub name: Identifier,
    pub docs: Vec<ContractDocComment>,
    pub attributes: Vec<ContractAttribute>,
    pub ty: ContractTypeRef,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractTypeRef {
    pub name: Identifier,
    pub arguments: Vec<ContractTypeRef>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractDocComment {
    pub text: String,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractAttribute {
    pub name: Identifier,
    pub arguments: Vec<ContractAttributeArgument>,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractAttributeArgument {
    pub name: Option<Identifier>,
    pub value: ContractAttributeValue,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractAttributeValue {
    Identifier(Identifier),
    String {
        raw: String,
        cooked: String,
        range: TextRange,
    },
    Integer {
        raw: String,
        range: TextRange,
    },
}

impl ContractAttributeValue {
    #[must_use]
    pub const fn range(&self) -> TextRange {
        match self {
            Self::Identifier(identifier) => identifier.range,
            Self::String { range, .. } | Self::Integer { range, .. } => *range,
        }
    }
}

/// Parses the one NIDL v2 grammar from a lossless [`SyntaxTree`].
///
/// Lexical and delimiter errors from the tree are returned before typed
/// parsing, so downstream crates receive one syntax-diagnostic stream.
/// Parses one Contract file's lossless [`SyntaxTree`] into the flat `ContractAst` model.
///
/// Lexical and delimiter errors from the tree are returned before typed parsing, so downstream
/// crates receive one syntax-diagnostic stream.
pub fn parse_contract_ast(tree: &SyntaxTree) -> Result<ContractAst, Vec<ContractAstError>> {
    if tree.language != SyntaxLanguage::Contract {
        return Err(vec![ContractAstError {
            range: tree.root.range,
            message: "a NIDL AST requires NIDL source".into(),
        }]);
    }
    if !tree.errors.is_empty() {
        return Err(tree
            .errors
            .iter()
            .map(|error| ContractAstError {
                range: error.range,
                message: error.message.clone(),
            })
            .collect());
    }
    Parser::new(tree).parse().map_err(|error| vec![error])
}

struct Parser<'a> {
    tree: &'a SyntaxTree,
    cursor: usize,
    previous: Option<TextRange>,
}

struct Prefix {
    docs: Vec<ContractDocComment>,
    attributes: Vec<ContractAttribute>,
    start: Option<TextRange>,
}

impl<'a> Parser<'a> {
    const fn new(tree: &'a SyntaxTree) -> Self {
        Self {
            tree,
            cursor: 0,
            previous: None,
        }
    }

    fn parse(mut self) -> Result<ContractAst, ContractAstError> {
        let header_prefix = self.prefix()?;
        let start = header_prefix.start.unwrap_or_else(|| self.current_range());
        self.expect_keyword(crate::Keyword::Contract, "expected `contract`")?;
        let name = self.identifier("contract name")?;
        // Flat header: `contract Name;` — every declaration after it belongs to the
        // contract until end of file.
        self.expect(TokenKind::Semicolon, "expected `;` after contract header")?;
        let mut items = Vec::new();
        loop {
            self.skip_ordinary_trivia();
            if self.at_end() {
                break;
            }
            let item_prefix = self.prefix()?;
            let item_start = item_prefix.start.unwrap_or_else(|| self.current_range());
            let item = match self.current_kind() {
                Some(TokenKind::Keyword(crate::Keyword::Handle)) => ContractItem::Handle(
                    self.handle(item_prefix.docs, item_prefix.attributes, item_start)?,
                ),
                Some(TokenKind::Keyword(crate::Keyword::Struct)) => ContractItem::Struct(
                    self.structure(item_prefix.docs, item_prefix.attributes, item_start)?,
                ),
                Some(TokenKind::Keyword(crate::Keyword::Enum)) => ContractItem::Enum(
                    self.enumeration(item_prefix.docs, item_prefix.attributes, item_start)?,
                ),
                Some(TokenKind::Keyword(crate::Keyword::Host)) => {
                    ContractItem::FunctionBlock(self.function_block(
                        ContractFunctionBlockKind::Host,
                        item_prefix.docs,
                        item_prefix.attributes,
                        item_start,
                    )?)
                }
                Some(TokenKind::Keyword(crate::Keyword::Nexa)) => {
                    ContractItem::FunctionBlock(self.function_block(
                        ContractFunctionBlockKind::Nexa,
                        item_prefix.docs,
                        item_prefix.attributes,
                        item_start,
                    )?)
                }
                _ => {
                    return Err(self.error_here("expected `handle`, `struct`, `enum`, `host`, or `nexa`"));
                }
            };
            items.push(item);
        }
        let end = self.previous_end().unwrap_or(start);
        let range = cover(start, end);
        Ok(ContractAst {
            source: self.tree.source.clone(),
            contract: ContractHeader {
                name,
                docs: header_prefix.docs,
                attributes: header_prefix.attributes,
                items,
                range,
            },
            range,
        })
    }

    fn handle(
        &mut self,
        docs: Vec<ContractDocComment>,
        attributes: Vec<ContractAttribute>,
        start: TextRange,
    ) -> Result<ContractHandleDecl, ContractAstError> {
        self.expect_keyword(crate::Keyword::Handle, "expected `handle`")?;
        let name = self.identifier("handle name")?;
        let end = self.expect(TokenKind::Semicolon, "expected `;` after handle")?;
        Ok(ContractHandleDecl {
            name,
            docs,
            attributes,
            range: cover(start, end),
        })
    }

    fn structure(
        &mut self,
        docs: Vec<ContractDocComment>,
        attributes: Vec<ContractAttribute>,
        start: TextRange,
    ) -> Result<ContractStructDecl, ContractAstError> {
        self.expect_keyword(crate::Keyword::Struct, "expected `struct`")?;
        let name = self.identifier("struct name")?;
        self.expect(TokenKind::LBrace, "expected `{` after struct name")?;
        let mut fields = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.at_end() {
                return Err(self.error_here("expected a struct field or `}`"));
            }
            let prefix = self.prefix()?;
            let field_start = prefix.start.unwrap_or_else(|| self.current_range());
            let field_name = self.identifier("field name")?;
            self.expect(TokenKind::Colon, "expected `:` after field name")?;
            let ty = self.ty()?;
            let end = if let Some(comma) = self.take(TokenKind::Comma) {
                comma
            } else if self.at(TokenKind::RBrace) {
                ty.range
            } else {
                return Err(self.error_here("expected `,` or `}` after struct field"));
            };
            fields.push(ContractField {
                name: field_name,
                docs: prefix.docs,
                attributes: prefix.attributes,
                ty,
                range: cover(field_start, end),
            });
        }
        let end = self.expect(TokenKind::RBrace, "expected `}` after struct")?;
        Ok(ContractStructDecl {
            name,
            docs,
            attributes,
            fields,
            range: cover(start, end),
        })
    }

    fn enumeration(
        &mut self,
        docs: Vec<ContractDocComment>,
        attributes: Vec<ContractAttribute>,
        start: TextRange,
    ) -> Result<ContractEnumDecl, ContractAstError> {
        self.expect_keyword(crate::Keyword::Enum, "expected `enum`")?;
        let name = self.identifier("enum name")?;
        self.expect(TokenKind::LBrace, "expected `{` after enum name")?;
        let mut variants = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.at_end() {
                return Err(self.error_here("expected an enum variant or `}`"));
            }
            let prefix = self.prefix()?;
            let variant_start = prefix.start.unwrap_or_else(|| self.current_range());
            let variant_name = self.identifier("variant name")?;
            let payload = if self.take(TokenKind::LParen).is_some() {
                let ty = self.ty()?;
                self.expect(TokenKind::RParen, "expected `)` after enum payload")?;
                Some(ty)
            } else {
                None
            };
            let payload_end = payload
                .as_ref()
                .map_or_else(|| variant_name.range, |ty| ty.range);
            let end = if let Some(comma) = self.take(TokenKind::Comma) {
                comma
            } else if self.at(TokenKind::RBrace) {
                payload_end
            } else {
                return Err(self.error_here("expected `,` or `}` after enum variant"));
            };
            variants.push(ContractVariant {
                name: variant_name,
                docs: prefix.docs,
                attributes: prefix.attributes,
                payload,
                range: cover(variant_start, end),
            });
        }
        let end = self.expect(TokenKind::RBrace, "expected `}` after enum")?;
        Ok(ContractEnumDecl {
            name,
            docs,
            attributes,
            variants,
            range: cover(start, end),
        })
    }

    fn function_block(
        &mut self,
        kind: ContractFunctionBlockKind,
        docs: Vec<ContractDocComment>,
        attributes: Vec<ContractAttribute>,
        start: TextRange,
    ) -> Result<ContractFunctionBlock, ContractAstError> {
        self.bump();
        self.expect(TokenKind::LBrace, "expected `{` after function block")?;
        let mut functions = Vec::new();
        while !self.at(TokenKind::RBrace) {
            if self.at_end() {
                return Err(self.error_here("expected a function or `}`"));
            }
            let prefix = self.prefix()?;
            let function_start = prefix.start.unwrap_or_else(|| self.current_range());
            functions.push(self.function(prefix.docs, prefix.attributes, function_start)?);
        }
        let end = self.expect(TokenKind::RBrace, "expected `}` after function block")?;
        Ok(ContractFunctionBlock {
            kind,
            docs,
            attributes,
            functions,
            range: cover(start, end),
        })
    }

    fn function(
        &mut self,
        docs: Vec<ContractDocComment>,
        attributes: Vec<ContractAttribute>,
        start: TextRange,
    ) -> Result<ContractFunction, ContractAstError> {
        let is_async = self.take_keyword(crate::Keyword::Async).is_some();
        self.expect_keyword(crate::Keyword::Fn, "expected `fn`")?;
        let name = self.identifier("function name")?;
        self.expect(TokenKind::LParen, "expected `(` after function name")?;
        let mut parameters = Vec::new();
        while !self.at(TokenKind::RParen) {
            if self.at_end() {
                return Err(self.error_here("expected a parameter or `)`"));
            }
            let prefix = self.prefix()?;
            let parameter_start = prefix.start.unwrap_or_else(|| self.current_range());
            let parameter_name = self.identifier("parameter name")?;
            self.expect(TokenKind::Colon, "expected `:` after parameter name")?;
            let ty = self.ty()?;
            let end = if let Some(comma) = self.take(TokenKind::Comma) {
                comma
            } else if self.at(TokenKind::RParen) {
                ty.range
            } else {
                return Err(self.error_here("expected `,` or `)` after parameter"));
            };
            parameters.push(ContractParameter {
                name: parameter_name,
                docs: prefix.docs,
                attributes: prefix.attributes,
                ty,
                range: cover(parameter_start, end),
            });
        }
        self.expect(TokenKind::RParen, "expected `)` after parameters")?;
        let result = if self.take(TokenKind::Arrow).is_some() {
            Some(self.ty()?)
        } else {
            None
        };
        let end = self.expect(TokenKind::Semicolon, "expected `;` after function")?;
        Ok(ContractFunction {
            name,
            docs,
            attributes,
            is_async,
            parameters,
            result,
            range: cover(start, end),
        })
    }

    fn ty(&mut self) -> Result<ContractTypeRef, ContractAstError> {
        let name = self.identifier("type name")?;
        let start = name.range;
        let mut arguments = Vec::new();
        let end = if self.take(TokenKind::Less).is_some() {
            while !self.at(TokenKind::Greater) {
                if self.at_end() {
                    return Err(self.error_here("expected a type argument or `>`"));
                }
                arguments.push(self.ty()?);
                if self.take(TokenKind::Comma).is_none() {
                    break;
                }
            }
            self.expect(TokenKind::Greater, "expected `>` after type arguments")?
        } else {
            name.range
        };
        Ok(ContractTypeRef {
            name,
            arguments,
            range: cover(start, end),
        })
    }

    fn prefix(&mut self) -> Result<Prefix, ContractAstError> {
        let mut docs = Vec::new();
        let mut attributes = Vec::new();
        let mut start = None;
        loop {
            self.skip_ordinary_trivia();
            if self.current_kind() == Some(TokenKind::DocComment) {
                let token = self.bump().expect("peeked doc comment");
                start.get_or_insert(token.range);
                docs.push(ContractDocComment {
                    text: self.text(token).to_owned(),
                    range: token.range,
                });
                continue;
            }
            if self.at(TokenKind::At) {
                let attribute = self.attribute()?;
                start.get_or_insert(attribute.range);
                attributes.push(attribute);
                continue;
            }
            break;
        }
        Ok(Prefix {
            docs,
            attributes,
            start,
        })
    }

    fn attribute(&mut self) -> Result<ContractAttribute, ContractAstError> {
        let start = self.expect(TokenKind::At, "expected `@`")?;
        let name = self.identifier("attribute name")?;
        self.expect(TokenKind::LParen, "expected `(` after attribute name")?;
        let mut arguments = Vec::new();
        while !self.at(TokenKind::RParen) {
            if self.at_end() {
                return Err(self.error_here("expected an attribute argument or `)`"));
            }
            let argument_start = self.current_range();
            let named = if self.named_argument_ahead() {
                let name = self.identifier("attribute argument name")?;
                self.expect(TokenKind::Equal, "expected `=` after argument name")?;
                Some(name)
            } else {
                None
            };
            let value = self.attribute_value()?;
            let value_end = value.range();
            let end = if let Some(comma) = self.take(TokenKind::Comma) {
                comma
            } else if self.at(TokenKind::RParen) {
                value_end
            } else {
                return Err(self.error_here("expected `,` or `)` after attribute argument"));
            };
            arguments.push(ContractAttributeArgument {
                name: named,
                value,
                range: cover(argument_start, end),
            });
        }
        let end = self.expect(TokenKind::RParen, "expected `)` after attribute arguments")?;
        Ok(ContractAttribute {
            name,
            arguments,
            range: cover(start, end),
        })
    }

    fn attribute_value(&mut self) -> Result<ContractAttributeValue, ContractAstError> {
        self.skip_ordinary_trivia();
        match self.current_kind() {
            Some(TokenKind::Identifier) => Ok(ContractAttributeValue::Identifier(
                self.identifier("attribute value")?,
            )),
            Some(TokenKind::Integer) => {
                let token = self.bump().expect("peeked integer");
                Ok(ContractAttributeValue::Integer {
                    raw: self.text(token).to_owned(),
                    range: token.range,
                })
            }
            Some(TokenKind::StringStart) => self.string_value(),
            _ => Err(self.error_here("expected an identifier, string, or integer")),
        }
    }

    fn string_value(&mut self) -> Result<ContractAttributeValue, ContractAstError> {
        let start = self.expect(TokenKind::StringStart, "expected string")?;
        let mut raw = String::new();
        while !self.at(TokenKind::StringEnd) {
            let Some(token) = self.bump() else {
                return Err(ContractAstError {
                    range: TextRange::new(start.start, self.tree.source.len()),
                    message: "unterminated string attribute argument".into(),
                });
            };
            if matches!(
                token.kind,
                TokenKind::InterpolationStart | TokenKind::InterpolationEnd
            ) {
                return Err(ContractAstError {
                    range: token.range,
                    message: "string interpolation is not allowed in NIDL attributes".into(),
                });
            }
            raw.push_str(self.text(token));
        }
        let end = self.expect(TokenKind::StringEnd, "expected closing quote")?;
        let cooked = decode_string(&raw).map_err(|message| ContractAstError {
            range: cover(start, end),
            message: format!("invalid string: {message}"),
        })?;
        Ok(ContractAttributeValue::String {
            raw,
            cooked,
            range: cover(start, end),
        })
    }

    fn identifier(&mut self, role: &str) -> Result<Identifier, ContractAstError> {
        self.skip_ordinary_trivia();
        let Some(token) = self.bump() else {
            return Err(self.error_here(&format!("expected {role}")));
        };
        if token.kind != TokenKind::Identifier {
            return Err(ContractAstError {
                range: token.range,
                message: format!("expected {role}"),
            });
        }
        Ok(Identifier {
            text: self.text(token).to_owned(),
            range: token.range,
        })
    }

    fn expect_keyword(
        &mut self,
        keyword: crate::Keyword,
        message: &str,
    ) -> Result<TextRange, ContractAstError> {
        self.expect(TokenKind::Keyword(keyword), message)
    }

    fn expect(&mut self, kind: TokenKind, message: &str) -> Result<TextRange, ContractAstError> {
        self.skip_ordinary_trivia();
        self.take(kind).ok_or_else(|| self.error_here(message))
    }

    fn take_keyword(&mut self, keyword: crate::Keyword) -> Option<TextRange> {
        self.take(TokenKind::Keyword(keyword))
    }

    fn take(&mut self, kind: TokenKind) -> Option<TextRange> {
        self.skip_ordinary_trivia();
        if self.current_kind() == Some(kind) {
            self.bump().map(|token| token.range)
        } else {
            None
        }
    }

    fn at(&mut self, kind: TokenKind) -> bool {
        self.skip_ordinary_trivia();
        self.current_kind() == Some(kind)
    }

    fn skip_ordinary_trivia(&mut self) {
        while self.tree.tokens.get(self.cursor).is_some_and(|token| {
            matches!(
                token.kind,
                TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
            )
        }) {
            self.cursor += 1;
        }
    }

    fn current_kind(&self) -> Option<TokenKind> {
        self.tree.tokens.get(self.cursor).map(|token| token.kind)
    }

    fn named_argument_ahead(&self) -> bool {
        if self.current_kind() != Some(TokenKind::Identifier) {
            return false;
        }
        self.tree.tokens[self.cursor + 1..]
            .iter()
            .find(|token| !token.kind.is_trivia())
            .is_some_and(|token| token.kind == TokenKind::Equal)
    }

    fn bump(&mut self) -> Option<Token> {
        let token = *self.tree.tokens.get(self.cursor)?;
        self.cursor += 1;
        self.previous = Some(token.range);
        Some(token)
    }

    fn previous_end(&self) -> Option<TextRange> {
        self.previous
    }

    fn current_range(&self) -> TextRange {
        self.tree.tokens.get(self.cursor).map_or_else(
            || TextRange::new(self.tree.source.len(), self.tree.source.len()),
            |token| token.range,
        )
    }

    fn text(&self, token: Token) -> &str {
        self.tree.token_text(&token)
    }

    fn error_here(&self, message: &str) -> ContractAstError {
        ContractAstError {
            range: self.current_range(),
            message: message.into(),
        }
    }

    fn at_end(&self) -> bool {
        self.cursor >= self.tree.tokens.len()
    }
}

fn decode_string(raw: &str) -> Result<String, &'static str> {
    let mut output = String::new();
    let mut characters = raw.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let Some(escape) = characters.next() else {
            return Err("trailing escape");
        };
        match escape {
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            '\\' => output.push('\\'),
            '"' => output.push('"'),
            _ => return Err("unsupported escape"),
        }
    }
    Ok(output)
}

fn cover(start: TextRange, end: TextRange) -> TextRange {
    TextRange::new(start.start, end.end)
}
