//! Recursive-descent parser for the Arkiv query language: text → [`Query`].
//!
//! The query language is part of the Arkiv *specification* (it defines the
//! database), so it lives here — host-agnostic, `no_std`, zero-dep. Parsing
//! produces a **typed** [`Query`] AST ([`AnnotVal`] carries `Uint`/`Str`/`Key`/
//! `Addr`, not host index bytes); turning those values into a particular index's
//! byte layout is the host's job, in the evaluator.
//!
//! Grammar:
//!
//! ```text
//! TopLevel  → '*' | '$all' | Or
//! Or        → And (('||' | 'OR') And)*
//! And       → Term (('&&' | 'AND') Term)*
//! Term      → '(' Or ')' | ('NOT' | '!') '(' Or ')' | Predicate
//! Predicate → Var '=' Value | Var '!=' Value
//!           | Var ('NOT')? 'IN' '(' Value+ ')'
//!           | Var ('>' | '>=' | '<' | '<=') Value
//!           | Var ('~' | '!~') StringLit          (pattern ends with '*')
//! Var       → Ident | '$owner' | '$creator' | '$key'
//!           | '$expiration' | '$contentType' | '$createdAtBlock'
//! Value     → Number | String | Address | EntityKey
//! ```
//!
//! Per-key value types are checked at parse time: `$owner`/`$creator` take an
//! address, `$key` an entity key (or a `0x…64hex` string), `$expiration`/
//! `$createdAtBlock` a number, `$contentType` a string; user keys accept any
//! literal. Range operators are rejected on address / entity-key values (no
//! meaningful ordering).

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use arkiv_interfaces::collections::NonEmptyVec;
use arkiv_interfaces::entity::annotations;
use arkiv_interfaces::primitives::{Address, EntityKey};
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn, Query};

use crate::lexer::{KEY_HEX_LEN, KEY_LEN, Token, hex_to_bytes, tokenize};

/// Why a query string failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Human-readable description.
    pub message: String,
    /// Byte offset into the input where the failure was detected, when known
    /// (lexer errors carry one; parser errors generally do not).
    pub position: Option<usize>,
}

impl ParseError {
    pub(crate) fn at(position: usize, message: &str) -> Self {
        Self {
            message: message.to_string(),
            position: Some(position),
        }
    }
    pub(crate) fn msg(message: &str) -> Self {
        Self {
            message: message.to_string(),
            position: None,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.position {
            Some(p) => write!(f, "query parse error at byte {p}: {}", self.message),
            None => write!(f, "query parse error: {}", self.message),
        }
    }
}

impl core::error::Error for ParseError {}

/// Parse a query string into a [`Query`] AST. See the [module docs](self) for the
/// grammar.
pub fn parse(input: &str) -> Result<Query, ParseError> {
    let tokens = tokenize(input)?;
    Parser::new(tokens).parse_top_level()
}

/// A literal value before it is typed against its key.
enum Literal {
    Number(u64),
    String(String),
    Address(Address),
    EntityKey(EntityKey),
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned()?;
        self.pos += 1;
        Some(t)
    }

    fn expect(&mut self, expected: &Token, what: &str) -> Result<(), ParseError> {
        match self.advance() {
            Some(t) if &t == expected => Ok(()),
            _ => Err(ParseError::msg(what)),
        }
    }

    fn parse_top_level(&mut self) -> Result<Query, ParseError> {
        // Standalone `*` / `$all` only at top level.
        let is_all = matches!(self.peek(), Some(Token::Star))
            || matches!(self.peek(), Some(Token::DollarTerm(n)) if n.as_bytes() == &annotations::ALL[1..]);
        if is_all {
            self.advance();
            if self.peek().is_some() {
                return Err(ParseError::msg("expected end of input after '*' / '$all'"));
            }
            return Ok(Query::All);
        }
        let q = self.parse_or()?;
        if self.peek().is_some() {
            return Err(ParseError::msg("unexpected trailing input"));
        }
        Ok(q)
    }

    fn parse_or(&mut self) -> Result<Query, ParseError> {
        let mut q = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.advance();
            let rhs = self.parse_and()?;
            q = Query::Or(Box::new(q), Box::new(rhs));
        }
        Ok(q)
    }

    fn parse_and(&mut self) -> Result<Query, ParseError> {
        let mut q = self.parse_term()?;
        while matches!(self.peek(), Some(Token::And)) {
            self.advance();
            let rhs = self.parse_term()?;
            q = Query::And(Box::new(q), Box::new(rhs));
        }
        Ok(q)
    }

    fn parse_term(&mut self) -> Result<Query, ParseError> {
        // `NOT (...)` — parens required so `NOT a = b` isn't ambiguous.
        if matches!(self.peek(), Some(Token::Not)) {
            self.advance();
            self.expect(&Token::LParen, "expected '(' after NOT")?;
            let inner = self.parse_or()?;
            self.expect(&Token::RParen, "expected ')' to close NOT group")?;
            return Ok(Query::Not(Box::new(inner)));
        }
        if matches!(self.peek(), Some(Token::LParen)) {
            self.advance();
            let inner = self.parse_or()?;
            self.expect(&Token::RParen, "expected ')' to close group")?;
            return Ok(inner);
        }
        self.parse_predicate()
    }

    fn parse_predicate(&mut self) -> Result<Query, ParseError> {
        let key = self.parse_annot_key()?;
        match self.peek() {
            Some(Token::Eq) => {
                self.advance();
                let value = self.parse_value(&key)?;
                Ok(Query::Eq { key, value })
            }
            Some(Token::Neq) => {
                self.advance();
                let value = self.parse_value(&key)?;
                Ok(Query::Neq { key, value })
            }
            Some(Token::Not) => {
                self.advance();
                self.expect(&Token::In, "expected IN after NOT in a predicate")?;
                let values = self.parse_value_list(&key)?;
                Ok(Query::NotIn { key, values })
            }
            Some(Token::In) => {
                self.advance();
                let values = self.parse_value_list(&key)?;
                Ok(Query::In { key, values })
            }
            Some(Token::Gt) => {
                let value = self.range_value(&key)?;
                Ok(Query::Gt { key, value })
            }
            Some(Token::Gte) => {
                let value = self.range_value(&key)?;
                Ok(Query::Gte { key, value })
            }
            Some(Token::Lt) => {
                let value = self.range_value(&key)?;
                Ok(Query::Lt { key, value })
            }
            Some(Token::Lte) => {
                let value = self.range_value(&key)?;
                Ok(Query::Lte { key, value })
            }
            Some(Token::Tilde) => {
                self.advance();
                let value = self.parse_glob_pattern(&key)?;
                Ok(Query::Glob { key, value })
            }
            Some(Token::NotTilde) => {
                self.advance();
                let value = self.parse_glob_pattern(&key)?;
                Ok(Query::NotGlob { key, value })
            }
            _ => Err(ParseError::msg(
                "expected an operator (=, !=, >, >=, <, <=, ~, !~, IN, NOT IN) after the key",
            )),
        }
    }

    /// Consume the (already-peeked) range operator and parse its value, rejecting
    /// value types with no meaningful ordering.
    fn range_value(&mut self, key: &AnnotKey) -> Result<AnnotVal, ParseError> {
        self.advance(); // the range operator
        if matches!(self.peek(), Some(Token::Address(_) | Token::EntityKey(_))) {
            return Err(ParseError::msg(
                "range operators (>, >=, <, <=) are not supported on address / entity-key values",
            ));
        }
        self.parse_value(key)
    }

    fn parse_annot_key(&mut self) -> Result<AnnotKey, ParseError> {
        match self.advance() {
            Some(Token::DollarTerm(name)) => builtin_from_dollar(&name)
                .map(AnnotKey::BuiltIn)
                .ok_or_else(|| ParseError::msg("unknown or non-queryable built-in field")),
            Some(Token::Ident(s)) => Ok(AnnotKey::User(s)),
            _ => Err(ParseError::msg("expected an annotation key")),
        }
    }

    fn parse_value(&mut self, key: &AnnotKey) -> Result<AnnotVal, ParseError> {
        let lit = self.parse_literal()?;
        value_for_key(key, lit)
    }

    fn parse_value_list(&mut self, key: &AnnotKey) -> Result<NonEmptyVec<AnnotVal>, ParseError> {
        self.expect(&Token::LParen, "expected '(' to start an IN value list")?;
        let mut vals = Vec::new();
        while !matches!(self.peek(), Some(Token::RParen)) {
            if self.peek().is_none() {
                return Err(ParseError::msg("unterminated IN value list"));
            }
            vals.push(self.parse_value(key)?);
        }
        self.advance(); // ')'
        NonEmptyVec::from_vec(vals)
            .ok_or_else(|| ParseError::msg("IN / NOT IN value list must be non-empty"))
    }

    fn parse_glob_pattern(&mut self, key: &AnnotKey) -> Result<AnnotVal, ParseError> {
        match key {
            AnnotKey::BuiltIn(BuiltIn::ContentType) | AnnotKey::User(_) => {}
            AnnotKey::BuiltIn(_) => {
                return Err(ParseError::msg(
                    "glob ('~' / '!~') is only supported on string-valued keys",
                ));
            }
        }
        let Literal::String(s) = self.parse_literal()? else {
            return Err(ParseError::msg("glob pattern must be a string literal"));
        };
        let Some(prefix) = s.strip_suffix('*') else {
            return Err(ParseError::msg("glob pattern must end in '*'"));
        };
        if prefix.contains('*') {
            return Err(ParseError::msg(
                "only a single trailing '*' is supported in a glob pattern",
            ));
        }
        Ok(AnnotVal::Str(prefix.as_bytes().to_vec()))
    }

    fn parse_literal(&mut self) -> Result<Literal, ParseError> {
        match self.advance() {
            Some(Token::Number(n)) => Ok(Literal::Number(n)),
            Some(Token::StringLit(s)) => Ok(Literal::String(s)),
            Some(Token::Address(a)) => Ok(Literal::Address(a)),
            Some(Token::EntityKey(b)) => Ok(Literal::EntityKey(b)),
            _ => Err(ParseError::msg("expected a literal value")),
        }
    }
}

/// Type a literal against its key, producing a spec [`AnnotVal`]. Per-key rules
/// mirror the write side; user keys accept any literal.
fn value_for_key(key: &AnnotKey, lit: Literal) -> Result<AnnotVal, ParseError> {
    Ok(match (key, lit) {
        (AnnotKey::BuiltIn(BuiltIn::Owner | BuiltIn::Creator), Literal::Address(a)) => {
            AnnotVal::Addr(a)
        }
        (AnnotKey::BuiltIn(BuiltIn::Owner | BuiltIn::Creator), _) => {
            return Err(ParseError::msg(
                "$owner / $creator require an address literal (0x + 40 hex)",
            ));
        }

        (AnnotKey::BuiltIn(BuiltIn::Key), Literal::EntityKey(b)) => AnnotVal::Key(b),
        (AnnotKey::BuiltIn(BuiltIn::Key), Literal::String(s)) => {
            AnnotVal::Key(decode_key_string(&s)?)
        }
        (AnnotKey::BuiltIn(BuiltIn::Key), _) => {
            return Err(ParseError::msg(
                "$key requires an entity-key literal (0x + 64 hex)",
            ));
        }

        (AnnotKey::BuiltIn(BuiltIn::Expiration | BuiltIn::CreatedAtBlock), Literal::Number(n)) => {
            AnnotVal::Uint(u64_to_be32(n))
        }
        (AnnotKey::BuiltIn(BuiltIn::Expiration | BuiltIn::CreatedAtBlock), _) => {
            return Err(ParseError::msg(
                "$expiration / $createdAtBlock require a number",
            ));
        }

        (AnnotKey::BuiltIn(BuiltIn::ContentType), Literal::String(s)) => {
            AnnotVal::Str(s.into_bytes())
        }
        (AnnotKey::BuiltIn(BuiltIn::ContentType), _) => {
            return Err(ParseError::msg("$contentType requires a string"));
        }

        (AnnotKey::User(_), Literal::Number(n)) => AnnotVal::Uint(u64_to_be32(n)),
        (AnnotKey::User(_), Literal::String(s)) => AnnotVal::Str(s.into_bytes()),
        (AnnotKey::User(_), Literal::Address(a)) => AnnotVal::Addr(a),
        (AnnotKey::User(_), Literal::EntityKey(b)) => AnnotVal::Key(b),
    })
}

/// Resolve a `$term` name (the text after the `$`) to a built-in field, if it is
/// one. Names come from the spec's [`annotations`] constants (minus the `$`), so
/// the parser and the store agree on the vocabulary. `$all` is deliberately not a
/// field — it is the all-selector, handled at the top level.
fn builtin_from_dollar(name: &str) -> Option<BuiltIn> {
    let n = name.as_bytes();
    if n == &annotations::OWNER[1..] {
        Some(BuiltIn::Owner)
    } else if n == &annotations::CREATOR[1..] {
        Some(BuiltIn::Creator)
    } else if n == &annotations::KEY[1..] {
        Some(BuiltIn::Key)
    } else if n == &annotations::EXPIRATION[1..] {
        Some(BuiltIn::Expiration)
    } else if n == &annotations::CONTENT_TYPE[1..] {
        Some(BuiltIn::ContentType)
    } else if n == &annotations::CREATED_AT_BLOCK[1..] {
        Some(BuiltIn::CreatedAtBlock)
    } else {
        None
    }
}

/// Decode a `0x…64hex` string into a 32-byte entity key (the JS SDK sends `$key`
/// values quoted).
fn decode_key_string(s: &str) -> Result<EntityKey, ParseError> {
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(|| ParseError::msg("$key string must be 0x-prefixed"))?;
    if stripped.len() != KEY_HEX_LEN {
        return Err(ParseError::msg("$key string must be a 32-byte hex key"));
    }
    let mut out = [0u8; KEY_LEN];
    hex_to_bytes(stripped, &mut out).map_err(|()| ParseError::msg("invalid hex in $key string"))?;
    Ok(out)
}

/// A `u64` as a 32-byte big-endian [`AnnotVal::Uint`] payload.
fn u64_to_be32(n: u64) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&n.to_be_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn nev(vals: Vec<AnnotVal>) -> NonEmptyVec<AnnotVal> {
        NonEmptyVec::from_vec(vals).unwrap()
    }

    #[test]
    fn all_selectors() {
        assert_eq!(parse("*").unwrap(), Query::All);
        assert_eq!(parse("$all").unwrap(), Query::All);
        assert!(parse("$all && a = 1").is_err()); // not valid mid-expression
    }

    #[test]
    fn user_eq_types_by_literal() {
        assert_eq!(
            parse("color = \"blue\"").unwrap(),
            Query::Eq {
                key: AnnotKey::User("color".to_string()),
                value: AnnotVal::Str(b"blue".to_vec()),
            }
        );
        assert_eq!(
            parse("age = 42").unwrap(),
            Query::Eq {
                key: AnnotKey::User("age".to_string()),
                value: AnnotVal::Uint(u64_to_be32(42)),
            }
        );
    }

    #[test]
    fn builtin_owner_takes_address() {
        let addr = "0x1111111111111111111111111111111111111111";
        assert_eq!(
            parse(&alloc::format!("$owner = {addr}")).unwrap(),
            Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Owner),
                value: AnnotVal::Addr([0x11; 20]),
            }
        );
        // Wrong literal type for a built-in is a parse error.
        assert!(parse("$owner = 42").is_err());
        assert!(parse("$expiration = \"foo\"").is_err());
    }

    #[test]
    fn builtin_key_accepts_hex_and_string() {
        let k = alloc::format!("0x{}", "ab".repeat(32)); // 64 hex
        let want = Query::Eq {
            key: AnnotKey::BuiltIn(BuiltIn::Key),
            value: AnnotVal::Key([0xab; 32]),
        };
        assert_eq!(parse(&alloc::format!("$key = {k}")).unwrap(), want);
        assert_eq!(parse(&alloc::format!("$key = \"{k}\"")).unwrap(), want);
    }

    #[test]
    fn expiration_number_is_uint() {
        assert_eq!(
            parse("$expiration = 100").unwrap(),
            Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Expiration),
                value: AnnotVal::Uint(u64_to_be32(100)),
            }
        );
    }

    #[test]
    fn in_and_not_in() {
        assert_eq!(
            parse("color IN (\"a\" \"b\")").unwrap(),
            Query::In {
                key: AnnotKey::User("color".to_string()),
                values: nev(vec![
                    AnnotVal::Str(b"a".to_vec()),
                    AnnotVal::Str(b"b".to_vec())
                ]),
            }
        );
        assert!(matches!(
            parse("color NOT IN (\"a\")").unwrap(),
            Query::NotIn { .. }
        ));
        assert!(parse("color IN ()").is_err());
    }

    #[test]
    fn ranges_drop_mode_and_reject_unordered() {
        assert_eq!(
            parse("$expiration > 50").unwrap(),
            Query::Gt {
                key: AnnotKey::BuiltIn(BuiltIn::Expiration),
                value: AnnotVal::Uint(u64_to_be32(50)),
            }
        );
        assert!(matches!(parse("n <= 9").unwrap(), Query::Lte { .. }));
        // No ordering on addresses / keys.
        let addr = "0x2222222222222222222222222222222222222222";
        assert!(parse(&alloc::format!("$owner > {addr}")).is_err());
    }

    #[test]
    fn glob_strips_star_and_is_string_only() {
        assert_eq!(
            parse("name ~ \"pre*\"").unwrap(),
            Query::Glob {
                key: AnnotKey::User("name".to_string()),
                value: AnnotVal::Str(b"pre".to_vec()),
            }
        );
        assert!(matches!(
            parse("name !~ \"p*\"").unwrap(),
            Query::NotGlob { .. }
        ));
        assert!(parse("name ~ \"nostar\"").is_err()); // must end with '*'
        assert!(parse("$expiration ~ \"x*\"").is_err()); // not string-valued
    }

    #[test]
    fn boolean_structure_and_precedence() {
        // AND binds tighter than OR: `a=1 || b=2 && c=3` → a=1 OR (b=2 AND c=3).
        let q = parse("a = 1 || b = 2 && c = 3").unwrap();
        assert!(matches!(q, Query::Or(_, rhs) if matches!(*rhs, Query::And(_, _))));
        assert!(matches!(parse("NOT (a = 1)").unwrap(), Query::Not(_)));
        assert!(matches!(parse("!(a = 1)").unwrap(), Query::Not(_)));
        // NOT requires parens.
        assert!(parse("NOT a = 1").is_err());
    }

    #[test]
    fn lexer_errors_surface_with_positions() {
        assert!(parse("\"unterminated").is_err());
        assert!(parse("a = 0xabc").is_err()); // hex not 40 or 64
        assert!(parse("$bogus = 1").is_err()); // unknown built-in
        let e = parse("a = @").unwrap_err();
        assert!(e.position.is_some());
    }

    #[test]
    fn neq_and_every_range_op() {
        assert!(matches!(parse("a != 1").unwrap(), Query::Neq { .. }));
        assert!(matches!(parse("a > 1").unwrap(), Query::Gt { .. }));
        assert!(matches!(parse("a >= 1").unwrap(), Query::Gte { .. }));
        assert!(matches!(parse("a < 1").unwrap(), Query::Lt { .. }));
        assert!(matches!(parse("a <= 1").unwrap(), Query::Lte { .. }));
    }

    #[test]
    fn boolean_associativity_and_grouping() {
        // AND tighter than OR, both left-associative.
        assert!(matches!(
            parse("a=1 && b=2 || c=3").unwrap(),
            Query::Or(l, _) if matches!(*l, Query::And(_, _))
        ));
        assert!(matches!(
            parse("a=1 || b=2 || c=3").unwrap(),
            Query::Or(l, _) if matches!(*l, Query::Or(_, _))
        ));
        assert!(matches!(
            parse("a=1 && b=2 && c=3").unwrap(),
            Query::And(l, _) if matches!(*l, Query::And(_, _))
        ));
        // Parens override precedence.
        assert!(matches!(
            parse("(a=1 || b=2) && c=3").unwrap(),
            Query::And(l, _) if matches!(*l, Query::Or(_, _))
        ));
        // Redundant nesting collapses.
        assert!(matches!(parse("((a = 1))").unwrap(), Query::Eq { .. }));
        assert!(matches!(
            parse("NOT (a=1 && b=2)").unwrap(),
            Query::Not(inner) if matches!(*inner, Query::And(_, _))
        ));
    }

    #[test]
    fn keyword_case_and_operator_aliases() {
        // `&&`==AND, `||`==OR, `!`==NOT, case-insensitive keywords.
        assert_eq!(parse("a=1 && b=2").unwrap(), parse("a=1 AND b=2").unwrap());
        assert_eq!(parse("a=1 && b=2").unwrap(), parse("a=1 and b=2").unwrap());
        assert_eq!(parse("a=1 || b=2").unwrap(), parse("a=1 Or b=2").unwrap());
        assert_eq!(parse("!(a=1)").unwrap(), parse("not (a=1)").unwrap());
        assert_eq!(parse("a in (1)").unwrap(), parse("a IN (1)").unwrap());
        assert_eq!(
            parse("a NOT IN (1)").unwrap(),
            parse("a not in (1)").unwrap()
        );
    }

    #[test]
    fn whitespace_is_insignificant() {
        assert_eq!(parse("   a   =   1   ").unwrap(), parse("a=1").unwrap());
        assert_eq!(parse("a=1\t&&\nb=2").unwrap(), parse("a=1 && b=2").unwrap());
    }

    #[test]
    fn string_escapes_and_empty() {
        assert_eq!(
            parse("x = \"a\\nb\\t\\\"\\\\\"").unwrap(),
            Query::Eq {
                key: AnnotKey::User("x".to_string()),
                value: AnnotVal::Str(b"a\nb\t\"\\".to_vec()),
            }
        );
        assert_eq!(
            parse("x = \"\"").unwrap(),
            Query::Eq {
                key: AnnotKey::User("x".to_string()),
                value: AnnotVal::Str(Vec::new()),
            }
        );
        assert!(parse("x = \"bad\\q\"").is_err()); // unknown escape
        assert!(parse("x = \"trailing\\").is_err()); // trailing backslash
    }

    #[test]
    fn number_bounds() {
        assert_eq!(
            parse("a = 0").unwrap(),
            Query::Eq {
                key: AnnotKey::User("a".to_string()),
                value: AnnotVal::Uint([0u8; 32]),
            }
        );
        // u64::MAX parses; one past it is a lex error.
        assert!(parse(&alloc::format!("a = {}", u64::MAX)).is_ok());
        assert!(parse("a = 18446744073709551616").is_err());
    }

    #[test]
    fn uint_layout_is_big_endian() {
        let Query::Eq {
            value: AnnotVal::Uint(bytes),
            ..
        } = parse("a = 258").unwrap()
        else {
            panic!("expected uint");
        };
        let mut want = [0u8; 32];
        want[30] = 0x01; // 258 = 0x0102
        want[31] = 0x02;
        assert_eq!(bytes, want);
    }

    #[test]
    fn every_builtin_type_check() {
        let addr = alloc::format!("0x{}", "cd".repeat(20));
        let key = alloc::format!("0x{}", "ab".repeat(32));
        assert!(matches!(
            parse(&alloc::format!("$creator = {addr}")).unwrap(),
            Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Creator),
                value: AnnotVal::Addr(_)
            }
        ));
        assert!(matches!(
            parse("$createdAtBlock = 7").unwrap(),
            Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::CreatedAtBlock),
                value: AnnotVal::Uint(_)
            }
        ));
        assert!(matches!(
            parse("$contentType = \"text/plain\"").unwrap(),
            Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::ContentType),
                value: AnnotVal::Str(_)
            }
        ));
        assert!(matches!(
            parse(&alloc::format!("$key = {key}")).unwrap(),
            Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Key),
                value: AnnotVal::Key(_)
            }
        ));
        // Each built-in rejects the wrong literal type.
        assert!(parse("$creator = 5").is_err());
        assert!(parse("$createdAtBlock = \"x\"").is_err());
        assert!(parse("$contentType = 5").is_err());
        assert!(parse("$key = 5").is_err());
        assert!(parse("$key = \"0xdead\"").is_err()); // short key string
    }

    #[test]
    fn user_key_accepts_any_literal() {
        let addr = alloc::format!("0x{}", "cd".repeat(20));
        let key = alloc::format!("0x{}", "ab".repeat(32));
        assert!(matches!(
            parse(&alloc::format!("who = {addr}")).unwrap(),
            Query::Eq {
                value: AnnotVal::Addr(_),
                ..
            }
        ));
        assert!(matches!(
            parse(&alloc::format!("ref = {key}")).unwrap(),
            Query::Eq {
                value: AnnotVal::Key(_),
                ..
            }
        ));
    }

    #[test]
    fn in_list_variants() {
        // Three values, then a single value.
        assert!(matches!(
            parse("c IN (1 2 3)").unwrap(),
            Query::In { values, .. } if values.len() == 3
        ));
        assert!(matches!(parse("c IN (1)").unwrap(), Query::In { .. }));
        // User key accepts a mixed-type list.
        let addr = alloc::format!("0x{}", "cd".repeat(20));
        assert_eq!(
            parse(&alloc::format!("m IN (1 \"two\" {addr})")).unwrap(),
            Query::In {
                key: AnnotKey::User("m".to_string()),
                values: nev(vec![
                    AnnotVal::Uint(u64_to_be32(1)),
                    AnnotVal::Str(b"two".to_vec()),
                    AnnotVal::Addr([0xcd; 20]),
                ]),
            }
        );
        // A built-in still type-checks each element.
        assert!(parse("$expiration IN (1 \"two\")").is_err());
    }

    #[test]
    fn glob_edges() {
        // Bare `*` → empty prefix (matches any value of that attribute).
        assert_eq!(
            parse("n ~ \"*\"").unwrap(),
            Query::Glob {
                key: AnnotKey::User("n".to_string()),
                value: AnnotVal::Str(Vec::new()),
            }
        );
        // Only a single trailing star.
        assert!(parse("n ~ \"a*b*\"").is_err());
        // $contentType (a built-in string) supports glob.
        assert!(matches!(
            parse("$contentType ~ \"text/*\"").unwrap(),
            Query::Glob {
                key: AnnotKey::BuiltIn(BuiltIn::ContentType),
                ..
            }
        ));
    }

    #[test]
    fn range_on_strings_ok_but_keys_rejected() {
        assert!(matches!(
            parse("name >= \"abc\"").unwrap(),
            Query::Gte {
                value: AnnotVal::Str(_),
                ..
            }
        ));
        let key = alloc::format!("0x{}", "ab".repeat(32));
        assert!(parse(&alloc::format!("ref < {key}")).is_err());
    }

    #[test]
    fn malformed_and_incomplete_inputs() {
        assert!(parse("").is_err()); // empty
        assert!(parse("a =").is_err()); // missing value
        assert!(parse("= 1").is_err()); // missing key
        assert!(parse("a").is_err()); // no operator
        assert!(parse("a = 1 &&").is_err()); // dangling connective
        assert!(parse("(a = 1").is_err()); // unbalanced paren
        assert!(parse("a = = 1").is_err()); // double operator
        assert!(parse("a = 1 b = 2").is_err()); // missing connective
    }

    #[test]
    fn any_dollar_term_lexes_parser_resolves_builtins() {
        // Every `$term` lexes; only the parser rejects unknown / non-field ones,
        // so adding a future built-in (e.g. `$recipient`) never touches the lexer.
        let e = parse("$recipient = 5").unwrap_err();
        assert!(
            e.position.is_none(),
            "unknown built-in is a parse error, not a lex error"
        );
        // `$all` is the all-selector, not a queryable field.
        assert!(parse("$all = 1").is_err());
        // The known built-ins still resolve.
        assert!(matches!(
            parse("$owner = 0x1111111111111111111111111111111111111111").unwrap(),
            Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Owner),
                ..
            }
        ));
    }
}
