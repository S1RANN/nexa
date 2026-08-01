use crate::{
    Keyword, SourceText, SourceTooLarge, SyntaxError, SyntaxErrorKind, TextRange, TextSize, Token,
    TokenKind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LexerLanguage {
    Nexa,
    Nidl,
}

/// Lossless tokens and recoverable lexical errors for one source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lexed {
    pub source: SourceText,
    pub tokens: Vec<Token>,
    pub errors: Vec<SyntaxError>,
}

impl Lexed {
    #[must_use]
    pub fn reconstructed(&self) -> String {
        let mut reconstructed = String::with_capacity(self.source.as_str().len());
        for token in &self.tokens {
            reconstructed.push_str(
                self.source
                    .slice(token.range)
                    .expect("lexer emits valid character-boundary ranges"),
            );
        }
        reconstructed
    }
}

pub fn lex_nexa(source: &str) -> Result<Lexed, SourceTooLarge> {
    Ok(Lexer::new(source, LexerLanguage::Nexa)?.lex())
}

pub fn lex_nidl(source: &str) -> Result<Lexed, SourceTooLarge> {
    Ok(Lexer::new(source, LexerLanguage::Nidl)?.lex())
}

struct Lexer {
    source: SourceText,
    language: LexerLanguage,
    cursor: usize,
    tokens: Vec<Token>,
    errors: Vec<SyntaxError>,
}

impl Lexer {
    fn new(source: &str, language: LexerLanguage) -> Result<Self, SourceTooLarge> {
        Ok(Self {
            source: SourceText::new(source)?,
            language,
            cursor: 0,
            tokens: Vec::new(),
            errors: Vec::new(),
        })
    }

    fn lex(mut self) -> Lexed {
        while !self.at_end() {
            self.lex_token(false);
        }
        debug_assert_eq!(self.reconstructed_len(), self.source.as_str().len());
        Lexed {
            source: self.source,
            tokens: self.tokens,
            errors: self.errors,
        }
    }

    fn lex_token(&mut self, interpolation: bool) {
        let start = self.cursor;
        let Some(character) = self.current_char() else {
            return;
        };

        if character.is_whitespace() {
            self.bump_while(char::is_whitespace);
            self.push(TokenKind::Whitespace, start, self.cursor);
            return;
        }

        if self.starts_with("///") {
            self.lex_line_comment(start, true);
            return;
        }
        if self.starts_with("//") {
            self.lex_line_comment(start, false);
            return;
        }
        if self.starts_with("/*") {
            self.lex_block_comment(start);
            return;
        }

        if character == '"' {
            self.lex_string();
            return;
        }
        if character == '\'' && self.language == LexerLanguage::Nexa {
            self.lex_rune();
            return;
        }
        if character.is_ascii_digit() {
            self.lex_number();
            return;
        }
        if character == '_' || character.is_ascii_alphabetic() {
            self.lex_word();
            return;
        }

        let pair = self.remaining().get(..2);
        let pair_kind = match pair {
            Some("->") => Some(TokenKind::Arrow),
            Some("=>") => Some(TokenKind::FatArrow),
            Some("==") => Some(TokenKind::EqualEqual),
            Some("!=") => Some(TokenKind::BangEqual),
            Some("<=") => Some(TokenKind::LessEqual),
            Some(">=") => Some(TokenKind::GreaterEqual),
            Some("&&") => Some(TokenKind::AmpAmp),
            Some("||") => Some(TokenKind::PipePipe),
            Some("::") => Some(TokenKind::ColonColon),
            Some("..") => Some(TokenKind::DotDot),
            _ => None,
        };
        if let Some(kind) = pair_kind {
            self.cursor += 2;
            self.push(kind, start, self.cursor);
            return;
        }

        self.bump_char();
        let kind = match character {
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => TokenKind::LBrace,
            '}' if interpolation => TokenKind::InterpolationEnd,
            '}' => TokenKind::RBrace,
            '[' => TokenKind::LBracket,
            ']' => TokenKind::RBracket,
            ':' => TokenKind::Colon,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semicolon,
            '+' => TokenKind::Plus,
            '-' => TokenKind::Minus,
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '%' => TokenKind::Percent,
            '=' => TokenKind::Equal,
            '!' => TokenKind::Bang,
            '<' => TokenKind::Less,
            '>' => TokenKind::Greater,
            '?' => TokenKind::Question,
            '@' => TokenKind::At,
            '.' => TokenKind::Dot,
            _ => {
                self.error(
                    SyntaxErrorKind::UnexpectedCharacter,
                    start,
                    self.cursor,
                    format!("unexpected character `{character}`"),
                );
                TokenKind::Unknown
            }
        };
        self.push(kind, start, self.cursor);
    }

    fn lex_line_comment(&mut self, start: usize, documentation: bool) {
        while self
            .current_char()
            .is_some_and(|character| character != '\n')
        {
            self.bump_char();
        }
        self.push(
            if documentation {
                TokenKind::DocComment
            } else {
                TokenKind::LineComment
            },
            start,
            self.cursor,
        );
    }

    fn lex_block_comment(&mut self, start: usize) {
        self.cursor += 2;
        let end = self
            .remaining()
            .find("*/")
            .map(|offset| self.cursor + offset);
        if let Some(end) = end {
            self.cursor = end + 2;
        } else {
            self.cursor = self.source.as_str().len();
        }
        self.push(TokenKind::BlockComment, start, self.cursor);
        if end.is_none() {
            self.error(
                SyntaxErrorKind::UnterminatedBlockComment,
                start,
                self.cursor,
                "unterminated block comment",
            );
        }
    }

    fn lex_number(&mut self) {
        let start = self.cursor;
        self.bump_while(|character| character.is_ascii_digit());
        let is_float = self.starts_with(".")
            && !self.starts_with("..")
            && self
                .remaining()
                .get(1..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|character| character.is_ascii_digit());
        if is_float {
            self.cursor += 1;
            self.bump_while(|character| character.is_ascii_digit());
        }
        self.push(
            if is_float {
                TokenKind::Float
            } else {
                TokenKind::Integer
            },
            start,
            self.cursor,
        );
    }

    fn lex_word(&mut self) {
        let start = self.cursor;
        self.bump_char();
        self.bump_while(|character| character == '_' || character.is_ascii_alphanumeric());
        let text = &self.source.as_str()[start..self.cursor];
        let kind = keyword(text, self.language).map_or(TokenKind::Identifier, TokenKind::Keyword);
        self.push(kind, start, self.cursor);
    }

    fn lex_rune(&mut self) {
        let start = self.cursor;
        self.bump_char();
        let mut scalar_count = 0_u8;
        let mut closed = false;
        while let Some(character) = self.current_char() {
            if character == '\'' {
                self.bump_char();
                closed = true;
                break;
            }
            if character == '\n' || character == '\r' {
                break;
            }
            if character == '\\' {
                let escape_start = self.cursor;
                self.bump_char();
                if let Some(escape) = self.current_char() {
                    self.bump_char();
                    if !matches!(escape, 'n' | 'r' | 't' | '\\' | '\'') {
                        self.error(
                            SyntaxErrorKind::InvalidEscape,
                            escape_start,
                            self.cursor,
                            format!("invalid rune escape `\\{escape}`"),
                        );
                    }
                }
                scalar_count = scalar_count.saturating_add(1);
            } else {
                self.bump_char();
                scalar_count = scalar_count.saturating_add(1);
            }
        }
        self.push(TokenKind::Rune, start, self.cursor);
        if !closed {
            self.error(
                SyntaxErrorKind::UnterminatedRune,
                start,
                self.cursor,
                "unterminated rune literal",
            );
        } else if scalar_count != 1 {
            self.error(
                SyntaxErrorKind::InvalidRune,
                start,
                self.cursor,
                "a rune literal must contain exactly one Unicode scalar",
            );
        }
    }

    fn lex_string(&mut self) {
        let quote = self.cursor;
        self.bump_char();
        self.push(TokenKind::StringStart, quote, self.cursor);
        let mut text_start = self.cursor;
        while !self.at_end() {
            if self.starts_with("\\${") {
                self.cursor += 3;
                continue;
            }
            if self.starts_with("${") {
                self.push_nonempty(TokenKind::StringText, text_start, self.cursor);
                let interpolation = self.cursor;
                self.cursor += 2;
                self.push(TokenKind::InterpolationStart, interpolation, self.cursor);
                self.lex_interpolation();
                text_start = self.cursor;
                continue;
            }
            let Some(character) = self.current_char() else {
                break;
            };
            if character == '"' {
                self.push_nonempty(TokenKind::StringText, text_start, self.cursor);
                let end = self.cursor;
                self.bump_char();
                self.push(TokenKind::StringEnd, end, self.cursor);
                return;
            }
            if character == '\\' {
                let escape_start = self.cursor;
                self.bump_char();
                if let Some(escape) = self.current_char() {
                    self.bump_char();
                    if !matches!(escape, 'n' | 'r' | 't' | '\\' | '"' | '$') {
                        self.error(
                            SyntaxErrorKind::InvalidEscape,
                            escape_start,
                            self.cursor,
                            format!("invalid string escape `\\{escape}`"),
                        );
                    }
                }
                continue;
            }
            self.bump_char();
        }
        self.push_nonempty(TokenKind::StringText, text_start, self.cursor);
        self.error(
            SyntaxErrorKind::UnterminatedString,
            quote,
            self.cursor,
            "unterminated string literal",
        );
    }

    fn lex_interpolation(&mut self) {
        let interpolation_start = self.cursor.saturating_sub(2);
        let mut brace_depth = 0_u32;
        while !self.at_end() {
            if self.starts_with("}") {
                if brace_depth == 0 {
                    let start = self.cursor;
                    self.bump_char();
                    self.push(TokenKind::InterpolationEnd, start, self.cursor);
                    return;
                }
                brace_depth -= 1;
                let start = self.cursor;
                self.bump_char();
                self.push(TokenKind::RBrace, start, self.cursor);
                continue;
            }
            if self.starts_with("{") {
                brace_depth = brace_depth.saturating_add(1);
                let start = self.cursor;
                self.bump_char();
                self.push(TokenKind::LBrace, start, self.cursor);
                continue;
            }
            self.lex_token(false);
        }
        self.error(
            SyntaxErrorKind::UnterminatedInterpolation,
            interpolation_start,
            self.cursor,
            "unterminated string interpolation",
        );
    }

    fn push(&mut self, kind: TokenKind, start: usize, end: usize) {
        debug_assert!(start <= end);
        debug_assert!(self.source.as_str().is_char_boundary(start));
        debug_assert!(self.source.as_str().is_char_boundary(end));
        self.tokens.push(Token {
            kind,
            range: range(start, end),
        });
    }

    fn push_nonempty(&mut self, kind: TokenKind, start: usize, end: usize) {
        if start < end {
            self.push(kind, start, end);
        }
    }

    fn error(
        &mut self,
        kind: SyntaxErrorKind,
        start: usize,
        end: usize,
        message: impl Into<String>,
    ) {
        self.errors.push(SyntaxError {
            kind,
            range: range(start, end),
            message: message.into(),
        });
    }

    fn current_char(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn bump_char(&mut self) {
        if let Some(character) = self.current_char() {
            self.cursor += character.len_utf8();
        }
    }

    fn bump_while(&mut self, predicate: impl Fn(char) -> bool) {
        while self.current_char().is_some_and(&predicate) {
            self.bump_char();
        }
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.remaining().starts_with(prefix)
    }

    fn remaining(&self) -> &str {
        &self.source.as_str()[self.cursor..]
    }

    fn at_end(&self) -> bool {
        self.cursor == self.source.as_str().len()
    }

    fn reconstructed_len(&self) -> usize {
        self.tokens
            .iter()
            .map(|token| token.range.len() as usize)
            .sum()
    }
}

fn range(start: usize, end: usize) -> TextRange {
    TextRange::new(
        TextSize::new(u32::try_from(start).expect("source length is checked")),
        TextSize::new(u32::try_from(end).expect("source length is checked")),
    )
}

fn keyword(text: &str, language: LexerLanguage) -> Option<Keyword> {
    match language {
        LexerLanguage::Nexa => nexa_keyword(text),
        LexerLanguage::Nidl => nidl_keyword(text),
    }
}

fn nexa_keyword(text: &str) -> Option<Keyword> {
    Some(match text {
        "fn" => Keyword::Fn,
        "async" => Keyword::Async,
        "return" => Keyword::Return,
        "let" => Keyword::Let,
        "mut" => Keyword::Mut,
        "if" => Keyword::If,
        "else" => Keyword::Else,
        "while" => Keyword::While,
        "match" => Keyword::Match,
        "new" => Keyword::New,
        "await" => Keyword::Await,
        "yield" => Keyword::Yield,
        "defer" => Keyword::Defer,
        "for" => Keyword::For,
        "struct" => Keyword::Struct,
        "enum" => Keyword::Enum,
        "class" => Keyword::Class,
        "use" => Keyword::Use,
        "as" => Keyword::As,
        "in" => Keyword::In,
        "true" => Keyword::True,
        "false" => Keyword::False,
        "pub" => Keyword::Pub,
        "package" => Keyword::Package,
        "const" => Keyword::Const,
        "break" => Keyword::Break,
        "continue" => Keyword::Continue,
        _ => return None,
    })
}

fn nidl_keyword(text: &str) -> Option<Keyword> {
    Some(match text {
        "contract" => Keyword::Contract,
        "host" => Keyword::Host,
        "nexa" => Keyword::Nexa,
        "handle" => Keyword::Handle,
        "struct" => Keyword::Struct,
        "enum" => Keyword::Enum,
        "async" => Keyword::Async,
        "fn" => Keyword::Fn,
        _ => return None,
    })
}
