//! Hand-rolled lexer for the Arkiv query language.
//!
//! Tokens:
//! - grouping and comparison: `(` `)` `=` `!=` `<` `<=` `>` `>=`
//! - keywords, case-insensitive: `AND` `OR` `NOT` `TRUE` `FALSE` `STARTSWITH`
//!   `EXISTS` `TYPEOF`. The last two are **reserved but unimplemented** — they
//!   lex so the parser can reject them by name instead of mistaking them for an
//!   attribute.
//! - typed literals: a [`TypeTag`] immediately followed by `(`, whose body is
//!   captured **raw** and validated later by [`literal`](crate::literal). Keeping
//!   the body raw is what lets one lexer handle bodies as different as `-5`,
//!   `3.5`, `0xAbC…` and `'it''s'` without a token per shape.
//! - `name` and `$sysName` identifiers, `'single-quoted'` strings, bare integers,
//!   and `*` (the all-selector).
//!
//! Whitespace is insignificant and `--` runs to end of line. Zero external deps:
//! this is part of the host-agnostic spec, so hex and string decoding are done by
//! hand.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::error::ParseError;

/// A literal's type tag — the `i32` in `i32(10)`.
///
/// Recognized **positionally** (before a `(`), not globally reserved, so the tag
/// list can grow without stealing identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeTag {
    I32,
    U64,
    U256,
    Dec,
    Str,
    Addr,
    Key,
    Bytes32,
    Bool,
}

impl TypeTag {
    /// The tag this name denotes, if any. Case-insensitive, like the keywords.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        const TAGS: [(&str, TypeTag); 9] = [
            ("i32", TypeTag::I32),
            ("u64", TypeTag::U64),
            ("u256", TypeTag::U256),
            ("dec", TypeTag::Dec),
            ("str", TypeTag::Str),
            ("addr", TypeTag::Addr),
            ("key", TypeTag::Key),
            ("bytes32", TypeTag::Bytes32),
            ("bool", TypeTag::Bool),
        ];
        TAGS.iter()
            .find(|(text, _)| name.eq_ignore_ascii_case(text))
            .map(|(_, tag)| *tag)
    }

    /// The spec's spelling of this tag.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::I32 => "i32",
            Self::U64 => "u64",
            Self::U256 => "u256",
            Self::Dec => "dec",
            Self::Str => "str",
            Self::Addr => "addr",
            Self::Key => "key",
            Self::Bytes32 => "bytes32",
            Self::Bool => "bool",
        }
    }
}

/// One lexed token. Identifiers and literals carry their text; typed-literal
/// bodies stay raw until [`literal`](crate::literal) validates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    LParen,
    RParen,
    And,
    Or,
    Not,
    StartsWith,
    /// Reserved, not implemented — the parser rejects it with a directive error.
    Exists,
    /// Reserved, not implemented — as [`Exists`](Self::Exists).
    TypeOf,
    Eq,
    /// Lexed only so the parser can explain why `!=` is not in the language.
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    /// `*` — matches every live entity.
    Star,
    True,
    False,
    /// A user attribute name.
    Name(String),
    /// A `$`-prefixed system attribute, carrying the name after the `$`.
    SysName(String),
    /// A typed literal: the tag, and the raw text between its parentheses.
    Tagged {
        tag: TypeTag,
        body: String,
        /// Byte offset of the body's first character, for literal errors.
        body_start: usize,
    },
    /// A bare integer, with any leading sign. System attributes only.
    Int(String),
    /// A `'…'` string with `''` escapes already resolved.
    Str(String),
}

/// A token and where it started, so every error can point at its cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpannedToken {
    pub token: Token,
    pub start: usize,
}

/// Tokenize an input string into the full token list.
pub(crate) fn tokenize(src: &str) -> Result<Vec<SpannedToken>, ParseError> {
    let mut lexer = Lexer::new(src);
    // Most queries are a handful of predicates; a modest reservation avoids
    // a couple of regrowths.
    let mut tokens = Vec::with_capacity(16);
    while let Some(token) = lexer.next_token()? {
        tokens.push(token);
    }
    Ok(tokens)
}

/// Scan a `'…'` literal starting at the opening quote, resolving `''` to one
/// quote. Returns the contents and the offset just past the closing quote.
///
/// Shared by the bare-string token and by the `str(…)` literal parser, so the
/// escape rule has exactly one definition.
pub(crate) fn scan_single_quoted(src: &str, open: usize) -> Result<(String, usize), ParseError> {
    let bytes = src.as_bytes();
    debug_assert_eq!(bytes.get(open), Some(&b'\''));
    let mut out = String::new();
    let mut index = open + 1;
    loop {
        if index >= src.len() {
            return Err(ParseError::syntax(
                open,
                "unterminated string literal — expected a closing '",
            ));
        }
        if bytes[index] == b'\'' {
            // A doubled quote is an escaped one; a lone quote ends the literal.
            if bytes.get(index + 1) == Some(&b'\'') {
                out.push('\'');
                index += 2;
                continue;
            }
            return Ok((out, index + 1));
        }
        let Some(ch) = src[index..].chars().next() else {
            return Err(ParseError::syntax(index, "invalid UTF-8 in string literal"));
        };
        out.push(ch);
        index += ch.len_utf8();
    }
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek_char(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn read_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    /// Skip whitespace and `--` comments until real input or end of file.
    fn skip_trivia(&mut self) {
        loop {
            while let Some(ch) = self.peek_char() {
                if ch.is_whitespace() {
                    self.read_char();
                } else {
                    break;
                }
            }
            if !self.rest().starts_with("--") {
                return;
            }
            while let Some(ch) = self.read_char() {
                if ch == '\n' {
                    break;
                }
            }
        }
    }

    /// Lex the next token, or `None` at end of input.
    fn next_token(&mut self) -> Result<Option<SpannedToken>, ParseError> {
        self.skip_trivia();
        let start = self.pos;
        let Some(ch) = self.peek_char() else {
            return Ok(None);
        };

        let token = match ch {
            '(' => {
                self.read_char();
                Token::LParen
            }
            ')' => {
                self.read_char();
                Token::RParen
            }
            '*' => {
                self.read_char();
                Token::Star
            }
            '=' => {
                self.read_char();
                Token::Eq
            }
            '!' => {
                self.read_char();
                if self.peek_char() == Some('=') {
                    self.read_char();
                    Token::Neq
                } else {
                    return Err(ParseError::syntax(
                        start,
                        "unexpected '!' — negation is written NOT",
                    ));
                }
            }
            '<' => {
                self.read_char();
                self.take_if_equals(Token::Lte, Token::Lt)
            }
            '>' => {
                self.read_char();
                self.take_if_equals(Token::Gte, Token::Gt)
            }
            '\'' => {
                let (content, end) = scan_single_quoted(self.src, self.pos)?;
                self.pos = end;
                Token::Str(content)
            }
            '$' => {
                self.read_char();
                let name = self.take_name_chars();
                if name.is_empty() {
                    return Err(ParseError::syntax(
                        start,
                        "expected a system attribute name after '$'",
                    ));
                }
                Token::SysName(name)
            }
            '&' | '|' | '~' => {
                return Err(ParseError::syntax(
                    start,
                    "symbol operators (&& || ~) were removed — use AND, OR, STARTSWITH",
                ));
            }
            _ if ch.is_ascii_digit() => {
                // `0x…` would otherwise lex as `0` followed by a name, and fail
                // much later with a confusing message.
                if self.rest().starts_with("0x") || self.rest().starts_with("0X") {
                    return Err(ParseError::syntax(
                        start,
                        "a hex literal must carry its type — write addr(0x…), key(0x…), \
                         bytes32(0x…) or u256(0x…)",
                    ));
                }
                Token::Int(self.take_int_chars())
            }
            '-' | '+' => {
                // A sign only starts a number; `--` was consumed as a comment and
                // a bare `-` inside a name is taken by `take_name_chars`.
                let signed = self.take_int_chars();
                if signed.len() <= 1 {
                    return Err(ParseError::syntax(start, "expected digits after the sign"));
                }
                Token::Int(signed)
            }
            _ if is_name_start(ch) => self.lex_word(start)?,
            _ => {
                return Err(ParseError::syntax(start, "unexpected character"));
            }
        };

        Ok(Some(SpannedToken { token, start }))
    }

    /// Consume a trailing `=` to pick the two-character operator.
    fn take_if_equals(&mut self, with_equals: Token, bare: Token) -> Token {
        if self.peek_char() == Some('=') {
            self.read_char();
            with_equals
        } else {
            bare
        }
    }

    /// A keyword, a typed literal, or a plain attribute name.
    fn lex_word(&mut self, start: usize) -> Result<Token, ParseError> {
        let word = self.take_name_chars();

        // Keywords win over everything, so they can never be attribute names.
        if let Some(keyword) = keyword_token(&word) {
            return Ok(keyword);
        }

        // A type tag only when a `(` follows — that is what "positionally
        // recognized" means, and it keeps the tag list out of the name space.
        if let Some(tag) = TypeTag::from_name(&word) {
            let after_word = self.pos;
            self.skip_trivia();
            if self.peek_char() == Some('(') {
                self.read_char();
                let (body, body_start) = self.take_tagged_body(start)?;
                return Ok(Token::Tagged {
                    tag,
                    body,
                    body_start,
                });
            }
            self.pos = after_word;
        }

        Ok(Token::Name(word))
    }

    /// The raw text between a typed literal's parentheses, and where it starts.
    ///
    /// Quoted sections are scanned rather than skipped character-wise, so a `)`
    /// inside `str('…')` doesn't end the literal early.
    fn take_tagged_body(&mut self, literal_start: usize) -> Result<(String, usize), ParseError> {
        let body_start = self.pos;
        let mut depth = 1usize;
        loop {
            let Some(ch) = self.peek_char() else {
                return Err(ParseError::syntax(
                    literal_start,
                    "unterminated typed literal — expected a closing ')'",
                ));
            };
            match ch {
                '\'' => {
                    let (_, end) = scan_single_quoted(self.src, self.pos)?;
                    self.pos = end;
                }
                '(' => {
                    depth += 1;
                    self.read_char();
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let body = self.src[body_start..self.pos].to_string();
                        self.read_char();
                        return Ok((body, body_start));
                    }
                    self.read_char();
                }
                _ => {
                    self.read_char();
                }
            }
        }
    }

    /// Consume a run of attribute-name characters.
    fn take_name_chars(&mut self) -> String {
        let start = self.pos;
        while let Some(ch) = self.peek_char() {
            if is_name_char(ch) {
                self.read_char();
            } else {
                break;
            }
        }
        self.src[start..self.pos].to_string()
    }

    /// Consume an optional sign followed by decimal digits.
    fn take_int_chars(&mut self) -> String {
        let start = self.pos;
        if matches!(self.peek_char(), Some('-' | '+')) {
            self.read_char();
        }
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_digit() {
                self.read_char();
            } else {
                break;
            }
        }
        self.src[start..self.pos].to_string()
    }
}

/// The reserved word `word` spells, if it is one. Case-insensitive.
fn keyword_token(word: &str) -> Option<Token> {
    if word.eq_ignore_ascii_case("and") {
        Some(Token::And)
    } else if word.eq_ignore_ascii_case("or") {
        Some(Token::Or)
    } else if word.eq_ignore_ascii_case("not") {
        Some(Token::Not)
    } else if word.eq_ignore_ascii_case("true") {
        Some(Token::True)
    } else if word.eq_ignore_ascii_case("false") {
        Some(Token::False)
    } else if word.eq_ignore_ascii_case("startswith") {
        Some(Token::StartsWith)
    } else if word.eq_ignore_ascii_case("exists") {
        Some(Token::Exists)
    } else if word.eq_ignore_ascii_case("typeof") {
        Some(Token::TypeOf)
    } else {
        None
    }
}

/// Whether `ch` may start an attribute name: `[A-Za-z]`, per the spec.
fn is_name_start(ch: char) -> bool {
    ch.is_ascii_alphabetic()
}

/// Whether `ch` may continue an attribute name: `[A-Za-z0-9_.\-]`.
fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ParseErrorKind;
    use alloc::vec;

    fn kinds(src: &str) -> Vec<Token> {
        tokenize(src)
            .unwrap()
            .into_iter()
            .map(|spanned| spanned.token)
            .collect()
    }

    #[test]
    fn comments_run_to_end_of_line() {
        assert_eq!(
            kinds("a -- this is ignored\n= true"),
            vec![Token::Name("a".into()), Token::Eq, Token::True],
        );
        // A comment with no trailing newline ends the input cleanly.
        assert_eq!(kinds("a -- trailing"), vec![Token::Name("a".into())]);
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            kinds("AND and And"),
            vec![Token::And, Token::And, Token::And]
        );
        assert_eq!(kinds("NOT not"), vec![Token::Not, Token::Not]);
        assert_eq!(
            kinds("startswith STARTSWITH"),
            vec![Token::StartsWith, Token::StartsWith]
        );
    }

    #[test]
    fn type_tags_need_a_paren_to_be_tags() {
        // With a paren it is a literal…
        assert!(matches!(
            kinds("i32(10)").as_slice(),
            [Token::Tagged {
                tag: TypeTag::I32,
                ..
            }]
        ));
        // …without one it is just a word (the parser rejects it as reserved).
        assert_eq!(kinds("i32"), vec![Token::Name("i32".into())]);
        // Whitespace between tag and paren is allowed.
        assert!(matches!(
            kinds("dec (3.5)").as_slice(),
            [Token::Tagged {
                tag: TypeTag::Dec,
                ..
            }]
        ));
    }

    #[test]
    fn tagged_body_is_captured_raw() {
        let Token::Tagged { body, .. } = &kinds("dec(-3.50)")[0] else {
            panic!("expected a tagged literal");
        };
        assert_eq!(body, "-3.50");
    }

    #[test]
    fn a_paren_inside_a_string_does_not_close_the_literal() {
        let Token::Tagged { body, .. } = &kinds("str('a)b')")[0] else {
            panic!("expected a tagged literal");
        };
        assert_eq!(body, "'a)b'");
    }

    #[test]
    fn doubled_quotes_escape() {
        assert_eq!(kinds("'it''s'"), vec![Token::Str("it's".into())]);
        assert_eq!(kinds("''"), vec![Token::Str(String::new())]);
    }

    #[test]
    fn names_take_dots_dashes_and_underscores() {
        assert_eq!(
            kinds("my.attr-name_2"),
            vec![Token::Name("my.attr-name_2".into())]
        );
        // A name may not start with a digit or a dash.
        assert!(tokenize("-name").is_err());
    }

    #[test]
    fn system_names_drop_the_dollar() {
        assert_eq!(kinds("$owner"), vec![Token::SysName("owner".into())]);
        assert!(tokenize("$ = 1").is_err());
    }

    #[test]
    fn signed_and_unsigned_bare_integers() {
        assert_eq!(kinds("1200000"), vec![Token::Int("1200000".into())]);
        assert_eq!(kinds("-42"), vec![Token::Int("-42".into())]);
        assert!(tokenize("- ").is_err());
    }

    /// A bare `0x…` would otherwise split into `0` and a name, so it is caught
    /// at the point of confusion and told which tag it wants.
    #[test]
    fn bare_hex_literals_are_rejected_with_the_tagged_forms() {
        let err = tokenize("$owner = 0xabcdef").unwrap_err();
        assert!(err.message.contains("addr(0x…)"), "{err}");
        assert_eq!(err.failure_position, Some(9));
        assert!(tokenize("h = 0Xdead").is_err());
        // A plain zero is still a number.
        assert_eq!(kinds("0"), vec![Token::Int("0".into())]);
    }

    #[test]
    fn comparison_operators() {
        assert_eq!(
            kinds("= != < <= > >="),
            vec![
                Token::Eq,
                Token::Neq,
                Token::Lt,
                Token::Lte,
                Token::Gt,
                Token::Gte
            ],
        );
    }

    #[test]
    fn removed_symbol_operators_are_named_in_the_error() {
        for src in ["a = 1 && b = 2", "a = 1 || b = 2", "a ~ 'x'"] {
            let err = tokenize(src).unwrap_err();
            assert!(err.message.contains("symbol operators"), "{src}: {err}");
        }
        // A lone `!` is not NOT any more.
        assert!(tokenize("!(a = true)").is_err());
    }

    #[test]
    fn unterminated_literals_report_their_opening_position() {
        let err = tokenize("name = str('oops").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::MalformedInputError);
        assert!(err.failure_position.is_some());
        let err = tokenize("name = str('ok'").unwrap_err();
        assert!(err.message.contains("closing ')'"), "{err}");
    }

    #[test]
    fn positions_point_at_the_token() {
        let tokens = tokenize("  a = true").unwrap();
        assert_eq!(tokens[0].start, 2);
        assert_eq!(tokens[1].start, 4);
        assert_eq!(tokens[2].start, 6);
    }
}
