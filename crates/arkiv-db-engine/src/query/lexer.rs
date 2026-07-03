//! Hand-rolled lexer for the Arkiv query language.
//!
//! Grammar tokens:
//!
//! - operators: `(`, `)`, `&&`, `||`, `=`, `!=`, `>`, `>=`, `<`, `<=`, `~`, `!~`, `*`
//! - keywords (case-insensitive): `AND`, `OR`, `NOT`, `IN`
//! - built-in idents: `$all`, `$owner`, `$creator`, `$key`, `$expiration`,
//!   `$contentType`, `$createdAtBlock`
//! - literals: `0x` + 64 hex (entity key), `0x` + 40 hex (address),
//!   `"..."` (string with `\\` / `\"` escapes), decimal number, Unicode
//!   identifier `[\p{L}_][\p{L}\p{N}_]*`
//!
//! Whitespace is skipped. Unknown bytes / malformed literals produce a
//! [`LexError`] with the byte offset where the failure was detected.

use eyre::{Result, bail};

/// One lexed token. Identifiers and literals carry their decoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
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
    /// `"..."` literal contents, with escapes resolved.
    StringLit(String),
    /// Decimal `[0-9]+`. Out-of-range numbers (> u64::MAX) are a lex error.
    Number(u64),
    /// User identifier. Reserved keywords (`AND`/`OR`/`NOT`/`IN`) and the
    /// built-in `$`-idents are returned as their own variants; only
    /// non-reserved idents reach this variant.
    Ident(String),
}

/// Tokenize an input string. Stops at end of input; trailing whitespace
/// is fine. Returns the full token list — small queries don't need
/// streaming.
pub fn tokenize(src: &str) -> Result<Vec<Token>> {
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

    fn next_token(&mut self) -> Result<Option<Token>> {
        self.skip_whitespace();
        let Some(c) = self.peek_char() else {
            return Ok(None);
        };

        // Single-char punctuation.
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

        // Two-char operators.
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
            // Bare `!` is treated as the NOT keyword for ergonomic
            // negation (`!(a = b)`).
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

        // String literal.
        if c == '"' {
            return self.lex_string().map(Some);
        }

        // Number, or `0x`-prefixed hex literal (Address / EntityKey).
        if c.is_ascii_digit() {
            return self.lex_number_or_hex().map(Some);
        }

        // Built-in `$ident`.
        if c == '$' {
            return self.lex_dollar_ident().map(Some);
        }

        // Identifier (Unicode-letter-led) or reserved word.
        if is_ident_start(c) {
            return self.lex_ident_or_keyword().map(Some);
        }

        bail!("lex error at byte {}: unexpected char {:?}", self.pos, c);
    }

    fn expect_pair(&mut self, first: char, second: char) -> Result<()> {
        let start = self.pos;
        let a = self.bump_char();
        let b = self.peek_char();
        if a != Some(first) || b != Some(second) {
            bail!(
                "lex error at byte {}: expected '{}{}'",
                start,
                first,
                second
            );
        }
        self.bump_char();
        Ok(())
    }

    fn lex_string(&mut self) -> Result<Token> {
        let start = self.pos;
        self.bump_char(); // opening "
        let mut out = String::new();
        loop {
            match self.bump_char() {
                None => bail!("lex error at byte {}: unterminated string literal", start),
                Some('"') => return Ok(Token::StringLit(out)),
                Some('\\') => match self.bump_char() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some(other) => bail!(
                        "lex error at byte {}: unknown escape '\\{}'",
                        self.pos - 1,
                        other
                    ),
                    None => bail!("lex error at byte {}: trailing backslash", self.pos),
                },
                Some(other) => out.push(other),
            }
        }
    }

    fn lex_number_or_hex(&mut self) -> Result<Token> {
        let start = self.pos;
        // Distinguish `0x…` from decimal.
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
                    let bytes = alloy_primitives::hex::decode(hex)
                        .map_err(|e| eyre::eyre!("hex decode at byte {start}: {e}"))?;
                    let mut a = [0u8; 20];
                    a.copy_from_slice(&bytes);
                    Ok(Token::Address(a))
                }
                64 => {
                    let bytes = alloy_primitives::hex::decode(hex)
                        .map_err(|e| eyre::eyre!("hex decode at byte {start}: {e}"))?;
                    let mut b = [0u8; 32];
                    b.copy_from_slice(&bytes);
                    Ok(Token::EntityKey(b))
                }
                n => {
                    bail!("lex error at byte {start}: hex literal must be 40 or 64 chars, got {n}")
                }
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
                .map_err(|e| eyre::eyre!("number parse at byte {start}: {e}"))?;
            Ok(Token::Number(n))
        }
    }

    fn lex_dollar_ident(&mut self) -> Result<Token> {
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
        let name = &self.src[name_start..self.pos];
        match name {
            "all" => Ok(Token::DollarAll),
            "owner" => Ok(Token::DollarOwner),
            "creator" => Ok(Token::DollarCreator),
            "key" => Ok(Token::DollarKey),
            "expiration" => Ok(Token::DollarExpiration),
            "contentType" => Ok(Token::DollarContentType),
            "createdAtBlock" => Ok(Token::DollarCreatedAtBlock),
            other => bail!("lex error at byte {start}: unknown built-in `${other}`"),
        }
    }

    fn lex_ident_or_keyword(&mut self) -> Result<Token> {
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                self.bump_char();
            } else {
                break;
            }
        }
        let s = &self.src[start..self.pos];
        // Reserved keywords match case-insensitively per the storage-
        // service convention (`AND`/`and`/`And` all valid).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn lex(src: &str) -> Vec<Token> {
        tokenize(src).expect("tokenize")
    }

    #[test]
    fn empty_input() {
        assert_eq!(lex(""), Vec::<Token>::new());
        assert_eq!(lex("   \t\n  "), Vec::<Token>::new());
    }

    #[test]
    fn punctuation() {
        assert_eq!(
            lex("()=*&&||!="),
            vec![
                Token::LParen,
                Token::RParen,
                Token::Eq,
                Token::Star,
                Token::And,
                Token::Or,
                Token::Neq,
            ]
        );
    }

    #[test]
    fn bang_alone_is_not() {
        assert_eq!(lex("!"), vec![Token::Not]);
        // Bang followed by non-`=` is still NOT.
        assert_eq!(lex("!("), vec![Token::Not, Token::LParen]);
    }

    #[test]
    fn keywords_case_insensitive() {
        assert_eq!(lex("AND and And"), vec![Token::And, Token::And, Token::And]);
        assert_eq!(lex("OR or"), vec![Token::Or, Token::Or]);
        assert_eq!(lex("NOT not"), vec![Token::Not, Token::Not]);
        assert_eq!(lex("IN in"), vec![Token::In, Token::In]);
    }

    #[test]
    fn builtin_dollar_idents() {
        assert_eq!(
            lex("$all $owner $creator $key $expiration $contentType $createdAtBlock"),
            vec![
                Token::DollarAll,
                Token::DollarOwner,
                Token::DollarCreator,
                Token::DollarKey,
                Token::DollarExpiration,
                Token::DollarContentType,
                Token::DollarCreatedAtBlock,
            ]
        );
    }

    #[test]
    fn unknown_dollar_ident_errors() {
        assert!(tokenize("$sequence").is_err());
        assert!(tokenize("$foo").is_err());
    }

    #[test]
    fn decimal_numbers() {
        assert_eq!(
            lex("0 1 42 18446744073709551615"),
            vec![
                Token::Number(0),
                Token::Number(1),
                Token::Number(42),
                Token::Number(u64::MAX),
            ]
        );
    }

    #[test]
    fn number_overflow_errors() {
        assert!(tokenize("18446744073709551616").is_err());
    }

    #[test]
    fn address_literal() {
        let toks = lex("0xaabbccddeeff00112233445566778899aabbccdd");
        assert_eq!(toks.len(), 1);
        match &toks[0] {
            Token::Address(a) => {
                assert_eq!(a[0], 0xaa);
                assert_eq!(a[19], 0xdd);
            }
            t => panic!("expected Address, got {t:?}"),
        }
    }

    #[test]
    fn entity_key_literal() {
        let toks = lex("0x1111111111111111111111111111111111111111000000000000000000000000");
        assert_eq!(toks.len(), 1);
        match &toks[0] {
            Token::EntityKey(b) => {
                assert_eq!(b[0], 0x11);
                assert_eq!(b[19], 0x11);
                assert_eq!(b[20], 0x00);
            }
            t => panic!("expected EntityKey, got {t:?}"),
        }
    }

    #[test]
    fn hex_uppercase_accepted() {
        assert!(matches!(
            tokenize("0xAABBCCDDEEFF00112233445566778899AABBCCDD")
                .expect("ok")
                .as_slice(),
            [Token::Address(_)]
        ));
    }

    #[test]
    fn hex_wrong_length_errors() {
        assert!(tokenize("0x1234").is_err());
        assert!(tokenize("0x").is_err());
    }

    #[test]
    fn string_literal_with_escapes() {
        assert_eq!(lex(r#""hello""#), vec![Token::StringLit("hello".into())]);
        assert_eq!(
            lex(r#""he said \"hi\"""#),
            vec![Token::StringLit(r#"he said "hi""#.into())]
        );
        assert_eq!(
            lex(r#""line\nbreak""#),
            vec![Token::StringLit("line\nbreak".into())]
        );
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(tokenize(r#""oops"#).is_err());
    }

    #[test]
    fn identifiers() {
        assert_eq!(
            lex("foo bar_baz x1"),
            vec![
                Token::Ident("foo".into()),
                Token::Ident("bar_baz".into()),
                Token::Ident("x1".into()),
            ]
        );
    }

    #[test]
    fn unicode_identifier() {
        assert_eq!(lex("café"), vec![Token::Ident("café".into())]);
    }

    #[test]
    fn realistic_query() {
        assert_eq!(
            lex(r#"$owner = 0xaabbccddeeff00112233445566778899aabbccdd && tag != "music""#),
            vec![
                Token::DollarOwner,
                Token::Eq,
                Token::Address([
                    0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                    0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                ]),
                Token::And,
                Token::Ident("tag".into()),
                Token::Neq,
                Token::StringLit("music".into()),
            ]
        );
    }

    #[test]
    fn unknown_char_errors() {
        assert!(tokenize("foo @ bar").is_err());
        assert!(tokenize("?").is_err());
    }
}
