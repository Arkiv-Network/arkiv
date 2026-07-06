//! Hand-rolled lexer for the Arkiv query language.
//!
//! Tokens:
//! - operators: `(`, `)`, `&&`, `||`, `=`, `!=`, `>`, `>=`, `<`, `<=`, `~`, `!~`, `*`
//! - keywords (case-insensitive): `AND`, `OR`, `NOT`, `IN`
//! - built-in idents: `$all`, `$owner`, `$creator`, `$key`, `$expiration`,
//!   `$contentType`, `$createdAtBlock`
//! - literals: `0x` + 64 hex (entity key), `0x` + 40 hex (address), `"…"`
//!   (string with `\\ \" \n \t \r` escapes), decimal number, and a
//!   Unicode-letter-led identifier.
//!
//! Whitespace is skipped; a bare `!` lexes as `NOT`. Zero external deps — this is
//! part of the host-agnostic spec, so hex decoding is done by hand.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::parse::ParseError;

/// One lexed token. Identifiers and literals carry their decoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    LParen,
    RParen,
    And,
    Or,
    Eq,
    Neq,
    Not,
    In,
    Star,
    Gt,
    Gte,
    Lt,
    Lte,
    /// `~` — prefix/glob match operator.
    Tilde,
    /// `!~` — negated prefix/glob match operator.
    NotTilde,

    DollarAll,
    DollarOwner,
    DollarCreator,
    DollarKey,
    DollarExpiration,
    DollarContentType,
    DollarCreatedAtBlock,

    /// `0x` + 64 hex chars, decoded to 32 bytes.
    EntityKey([u8; 32]),
    /// `0x` + 40 hex chars, decoded to 20 bytes.
    Address([u8; 20]),
    /// `"…"` literal contents, escapes resolved.
    StringLit(String),
    /// Decimal `[0-9]+`, `<= u64::MAX`.
    Number(u64),
    /// User identifier (reserved words and `$`-idents get their own variants).
    Ident(String),
}

/// Tokenize an input string into the full token list.
pub(crate) fn tokenize(src: &str) -> Result<Vec<Token>, ParseError> {
    let mut lex = Lexer::new(src);
    let mut out = Vec::new();
    while let Some(tok) = lex.next_token()? {
        out.push(tok);
    }
    Ok(out)
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

    fn bump_char(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek_char() {
            if c.is_whitespace() {
                self.bump_char();
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> Result<Option<Token>, ParseError> {
        self.skip_whitespace();
        let Some(c) = self.peek_char() else {
            return Ok(None);
        };

        match c {
            '(' => {
                self.bump_char();
                return Ok(Some(Token::LParen));
            }
            ')' => {
                self.bump_char();
                return Ok(Some(Token::RParen));
            }
            '*' => {
                self.bump_char();
                return Ok(Some(Token::Star));
            }
            '=' => {
                self.bump_char();
                return Ok(Some(Token::Eq));
            }
            _ => {}
        }

        if c == '&' {
            return self.expect_pair('&', '&').map(|()| Some(Token::And));
        }
        if c == '|' {
            return self.expect_pair('|', '|').map(|()| Some(Token::Or));
        }
        if c == '!' {
            self.bump_char();
            if self.peek_char() == Some('=') {
                self.bump_char();
                return Ok(Some(Token::Neq));
            }
            if self.peek_char() == Some('~') {
                self.bump_char();
                return Ok(Some(Token::NotTilde));
            }
            // Bare `!` is the NOT keyword (`!(a = b)`).
            return Ok(Some(Token::Not));
        }
        if c == '>' {
            self.bump_char();
            if self.peek_char() == Some('=') {
                self.bump_char();
                return Ok(Some(Token::Gte));
            }
            return Ok(Some(Token::Gt));
        }
        if c == '<' {
            self.bump_char();
            if self.peek_char() == Some('=') {
                self.bump_char();
                return Ok(Some(Token::Lte));
            }
            return Ok(Some(Token::Lt));
        }
        if c == '~' {
            self.bump_char();
            return Ok(Some(Token::Tilde));
        }
        if c == '"' {
            return self.lex_string().map(Some);
        }
        if c.is_ascii_digit() {
            return self.lex_number_or_hex().map(Some);
        }
        if c == '$' {
            return self.lex_dollar_ident().map(Some);
        }
        if is_ident_start(c) {
            return self.lex_ident_or_keyword().map(Some);
        }

        Err(ParseError::at(self.pos, "unexpected character"))
    }

    fn expect_pair(&mut self, first: char, second: char) -> Result<(), ParseError> {
        let start = self.pos;
        let a = self.bump_char();
        let b = self.peek_char();
        if a != Some(first) || b != Some(second) {
            return Err(ParseError::at(start, "expected a two-character operator"));
        }
        self.bump_char();
        Ok(())
    }

    fn lex_string(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        self.bump_char(); // opening "
        let mut out = String::new();
        loop {
            match self.bump_char() {
                None => return Err(ParseError::at(start, "unterminated string literal")),
                Some('"') => return Ok(Token::StringLit(out)),
                Some('\\') => match self.bump_char() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some(_) => return Err(ParseError::at(self.pos - 1, "unknown string escape")),
                    None => return Err(ParseError::at(self.pos, "trailing backslash in string")),
                },
                Some(other) => out.push(other),
            }
        }
    }

    fn lex_number_or_hex(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        if self.rest().starts_with("0x") || self.rest().starts_with("0X") {
            self.pos += 2;
            let hex_start = self.pos;
            while let Some(c) = self.peek_char() {
                if c.is_ascii_hexdigit() {
                    self.bump_char();
                } else {
                    break;
                }
            }
            let hex = &self.src[hex_start..self.pos];
            match hex.len() {
                40 => {
                    let mut a = [0u8; 20];
                    hex_to_bytes(hex, &mut a)
                        .map_err(|()| ParseError::at(start, "invalid address hex"))?;
                    Ok(Token::Address(a))
                }
                64 => {
                    let mut b = [0u8; 32];
                    hex_to_bytes(hex, &mut b)
                        .map_err(|()| ParseError::at(start, "invalid entity-key hex"))?;
                    Ok(Token::EntityKey(b))
                }
                _ => Err(ParseError::at(
                    start,
                    "hex literal must be 40 (address) or 64 (entity key) chars",
                )),
            }
        } else {
            while let Some(c) = self.peek_char() {
                if c.is_ascii_digit() {
                    self.bump_char();
                } else {
                    break;
                }
            }
            let s = &self.src[start..self.pos];
            let n: u64 = s
                .parse()
                .map_err(|_| ParseError::at(start, "number out of range (max u64)"))?;
            Ok(Token::Number(n))
        }
    }

    fn lex_dollar_ident(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        self.bump_char(); // consume '$'
        let name_start = self.pos;
        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                self.bump_char();
            } else {
                break;
            }
        }
        match &self.src[name_start..self.pos] {
            "all" => Ok(Token::DollarAll),
            "owner" => Ok(Token::DollarOwner),
            "creator" => Ok(Token::DollarCreator),
            "key" => Ok(Token::DollarKey),
            "expiration" => Ok(Token::DollarExpiration),
            "contentType" => Ok(Token::DollarContentType),
            "createdAtBlock" => Ok(Token::DollarCreatedAtBlock),
            _ => Err(ParseError::at(start, "unknown built-in annotation")),
        }
    }

    fn lex_ident_or_keyword(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                self.bump_char();
            } else {
                break;
            }
        }
        let s = &self.src[start..self.pos];
        // Reserved keywords match case-insensitively (`AND`/`and`/`And`).
        if s.eq_ignore_ascii_case("and") {
            Ok(Token::And)
        } else if s.eq_ignore_ascii_case("or") {
            Ok(Token::Or)
        } else if s.eq_ignore_ascii_case("not") {
            Ok(Token::Not)
        } else if s.eq_ignore_ascii_case("in") {
            Ok(Token::In)
        } else {
            Ok(Token::Ident(s.to_string()))
        }
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Decode `hex` into `out`; errors if the length doesn't match `out` or a
/// character isn't a hex digit.
pub(crate) fn hex_to_bytes(hex: &str, out: &mut [u8]) -> Result<(), ()> {
    let bytes = hex.as_bytes();
    if bytes.len() != out.len() * 2 {
        return Err(());
    }
    for (i, dst) in out.iter_mut().enumerate() {
        let hi = hex_nibble(bytes[2 * i]).ok_or(())?;
        let lo = hex_nibble(bytes[2 * i + 1]).ok_or(())?;
        *dst = (hi << 4) | lo;
    }
    Ok(())
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
