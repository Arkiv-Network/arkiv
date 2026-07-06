//! Hand-rolled lexer for the Arkiv query language.
//!
//! Tokens:
//! - operators: `(`, `)`, `&&`, `||`, `=`, `!=`, `>`, `>=`, `<`, `<=`, `~`, `!~`, `*`
//! - keywords (case-insensitive): `AND`, `OR`, `NOT`, `IN`
//! - `$term` — any `$`-prefixed identifier (e.g. `$owner`, `$expiration`). The
//!   lexer captures the name only; which names are valid built-ins is resolved by
//!   the parser, so adding a built-in never touches the grammar.
//! - literals: `0x` + hex (an address or an entity key, by length), `"…"`
//!   (string with `\\ \" \n \t \r` escapes), decimal number, and a
//!   Unicode-letter-led identifier.
//!
//! Whitespace is skipped; a bare `!` lexes as `NOT`. Zero external deps — this is
//! part of the host-agnostic spec, so hex decoding is done by hand.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use arkiv_interfaces::primitives::{Address, EntityKey};

use crate::parse::ParseError;

/// Byte length of an address and of an entity key, taken from the spec primitives
/// so the hex-literal lengths below aren't magic numbers.
pub(crate) const ADDRESS_LEN: usize = core::mem::size_of::<Address>();
pub(crate) const KEY_LEN: usize = core::mem::size_of::<EntityKey>();
/// Hex-character length of each — two hex chars per byte.
pub(crate) const ADDRESS_HEX_LEN: usize = ADDRESS_LEN * 2;
pub(crate) const KEY_HEX_LEN: usize = KEY_LEN * 2;

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

    /// A `$`-prefixed term, carrying the name after the `$` (e.g. `owner`). The
    /// parser decides which names are valid built-ins.
    DollarTerm(String),

    /// `0x` + [`KEY_HEX_LEN`] hex chars, decoded to an entity key.
    EntityKey(EntityKey),
    /// `0x` + [`ADDRESS_HEX_LEN`] hex chars, decoded to an address.
    Address(Address),
    /// `"…"` literal contents, escapes resolved.
    StringLit(String),
    /// Decimal `[0-9]+`, `<= u64::MAX`.
    Number(u64),
    /// User identifier (reserved words and `$`-terms get their own variants).
    Ident(String),
}

/// Tokenize an input string into the full token list.
pub(crate) fn tokenize(src: &str) -> Result<Vec<Token>, ParseError> {
    let mut lex = Lexer::new(src);
    // Most queries are small; a modest reservation avoids a couple of regrowths.
    let mut out = Vec::with_capacity(16);
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

    /// The not-yet-consumed input.
    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    /// The next character without consuming it.
    fn peek_char(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Consume and return the next character.
    fn read_char(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// Skip any run of whitespace.
    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek_char() {
            if c.is_whitespace() {
                self.read_char();
            } else {
                break;
            }
        }
    }

    /// Lex the next token, or `None` at end of input.
    fn next_token(&mut self) -> Result<Option<Token>, ParseError> {
        self.skip_whitespace();
        let Some(c) = self.peek_char() else {
            return Ok(None);
        };

        match c {
            '(' => {
                self.read_char();
                return Ok(Some(Token::LParen));
            }
            ')' => {
                self.read_char();
                return Ok(Some(Token::RParen));
            }
            '*' => {
                self.read_char();
                return Ok(Some(Token::Star));
            }
            '=' => {
                self.read_char();
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
            self.read_char();
            if self.peek_char() == Some('=') {
                self.read_char();
                return Ok(Some(Token::Neq));
            }
            if self.peek_char() == Some('~') {
                self.read_char();
                return Ok(Some(Token::NotTilde));
            }
            // Bare `!` is the NOT keyword (`!(a = b)`).
            return Ok(Some(Token::Not));
        }
        if c == '>' {
            self.read_char();
            if self.peek_char() == Some('=') {
                self.read_char();
                return Ok(Some(Token::Gte));
            }
            return Ok(Some(Token::Gt));
        }
        if c == '<' {
            self.read_char();
            if self.peek_char() == Some('=') {
                self.read_char();
                return Ok(Some(Token::Lte));
            }
            return Ok(Some(Token::Lt));
        }
        if c == '~' {
            self.read_char();
            return Ok(Some(Token::Tilde));
        }
        if c == '"' {
            return self.lex_string().map(Some);
        }
        if c.is_ascii_digit() {
            return self.lex_number_or_hex().map(Some);
        }
        if c == '$' {
            return Ok(Some(self.lex_dollar_term()));
        }
        if is_ident_start(c) {
            return self.lex_ident_or_keyword().map(Some);
        }

        Err(ParseError::at(self.pos, "unexpected character"))
    }

    /// Consume a two-character operator, erroring if the pair doesn't match.
    fn expect_pair(&mut self, first: char, second: char) -> Result<(), ParseError> {
        let start = self.pos;
        let a = self.read_char();
        let b = self.peek_char();
        if a != Some(first) || b != Some(second) {
            return Err(ParseError::at(start, "expected a two-character operator"));
        }
        self.read_char();
        Ok(())
    }

    /// Lex a `"…"` string literal, resolving escapes.
    fn lex_string(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        self.read_char(); // opening "
        let mut out = String::new();
        loop {
            match self.read_char() {
                None => return Err(ParseError::at(start, "unterminated string literal")),
                Some('"') => return Ok(Token::StringLit(out)),
                Some('\\') => match self.read_char() {
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

    /// Lex a decimal number, or a `0x`-prefixed hex address / entity-key literal.
    fn lex_number_or_hex(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        if self.rest().starts_with("0x") || self.rest().starts_with("0X") {
            self.pos += 2;
            let hex_start = self.pos;
            while let Some(c) = self.peek_char() {
                if c.is_ascii_hexdigit() {
                    self.read_char();
                } else {
                    break;
                }
            }
            let hex = &self.src[hex_start..self.pos];
            match hex.len() {
                ADDRESS_HEX_LEN => {
                    let mut a: Address = [0u8; ADDRESS_LEN];
                    hex_to_bytes(hex, &mut a)
                        .map_err(|()| ParseError::at(start, "invalid address hex"))?;
                    Ok(Token::Address(a))
                }
                KEY_HEX_LEN => {
                    let mut b: EntityKey = [0u8; KEY_LEN];
                    hex_to_bytes(hex, &mut b)
                        .map_err(|()| ParseError::at(start, "invalid entity-key hex"))?;
                    Ok(Token::EntityKey(b))
                }
                _ => Err(ParseError::at(
                    start,
                    "hex literal must be an address or an entity key",
                )),
            }
        } else {
            while let Some(c) = self.peek_char() {
                if c.is_ascii_digit() {
                    self.read_char();
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

    /// Lex a `$`-prefixed term, returning the name after the `$`. Validity of the
    /// name is the parser's concern.
    fn lex_dollar_term(&mut self) -> Token {
        self.read_char(); // consume '$'
        let name_start = self.pos;
        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                self.read_char();
            } else {
                break;
            }
        }
        Token::DollarTerm(self.src[name_start..self.pos].to_string())
    }

    /// Lex a bare identifier, or a (case-insensitive) reserved keyword.
    fn lex_ident_or_keyword(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                self.read_char();
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

/// Whether `c` may start an identifier.
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// Whether `c` may continue an identifier.
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

/// A single hex character to its 0–15 value.
fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
