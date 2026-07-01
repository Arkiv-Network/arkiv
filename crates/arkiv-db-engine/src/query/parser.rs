//! Recursive-descent parser for the Arkiv query language.
//!
//! Grammar:
//!
//! ```text
//! TopLevel  → '*' | '$all' | Or
//! Or        → And (('||' | 'OR') And)*
//! And       → Term (('&&' | 'AND') Term)*
//! Term      → '(' Or ')'
//!           | ('NOT' | '!') '(' Or ')'
//!           | Predicate
//! Predicate → Var '=' Value
//!           | Var '!=' Value
//!           | Var ('NOT')? 'IN' '(' Value+ ')'
//!           | Var '>'  Value
//!           | Var '>=' Value
//!           | Var '<'  Value
//!           | Var '<=' Value
//!           | Var '~'  StringLit    (pattern must end with '*')
//!           | Var '!~' StringLit
//! Var       → Ident | '$owner' | '$creator' | '$key'
//!           | '$expiration' | '$contentType' | '$createdAtBlock'
//! Value     → Number | String | Address | EntityKey
//! ```
//!
//! Literal-to-pair-value encoding happens at parse time so the
//! evaluator does no per-key arithmetic — see [`encode_for_key`] for
//! the matrix. Per-key type mismatches (e.g. `$expiration = "foo"`)
//! are parse errors.

use eyre::{Result, bail};

use super::lexer::{Token, tokenize};
use crate::AnnotMode;

/// Parsed query AST. `Not` stays in the tree (no DeMorgan / DNF pass);
/// the evaluator handles it as `$all \ eval(inner)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// `*` or `$all` — every live entity.
    All,
    /// Leaf equality: `key = value`.
    Eq { key: AnnotKey, value: AnnotVal },
    /// Leaf inequality: `key != value`.
    Neq { key: AnnotKey, value: AnnotVal },
    /// Leaf inclusion: `key IN (v1 v2 …)`. `values` is non-empty.
    In {
        key: AnnotKey,
        values: Vec<AnnotVal>,
    },
    /// Leaf negated inclusion: `key NOT IN (...)`. `values` is non-empty.
    NotIn {
        key: AnnotKey,
        values: Vec<AnnotVal>,
    },
    /// Leaf range: `key > value`. `mode` selects int vs string index.
    Gt { key: AnnotKey, value: AnnotVal, mode: AnnotMode },
    /// Leaf range: `key >= value`.
    Gte { key: AnnotKey, value: AnnotVal, mode: AnnotMode },
    /// Leaf range: `key < value`.
    Lt { key: AnnotKey, value: AnnotVal, mode: AnnotMode },
    /// Leaf range: `key <= value`.
    Lte { key: AnnotKey, value: AnnotVal, mode: AnnotMode },
    /// Leaf glob: `key ~ "prefix*"`. `value` contains the prefix bytes
    /// (the trailing `*` is stripped at parse time). Always `Str` mode.
    Glob { key: AnnotKey, value: AnnotVal },
    /// Leaf negated glob: `key !~ "prefix*"`. Evaluates as
    /// `$all \ eval(Glob { key, value })`.
    NotGlob { key: AnnotKey, value: AnnotVal },
    /// Logical AND.
    And(Box<Query>, Box<Query>),
    /// Logical OR.
    Or(Box<Query>, Box<Query>),
    /// Logical NOT. Always wraps a parenthesized sub-expression at the
    /// source level (`NOT (...)`); the parens are not represented in
    /// the AST.
    Not(Box<Query>),
}

/// Annotation key — built-in `$`-idents are distinguished from user
/// idents so the encoder can pick the right value layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotKey {
    BuiltIn(BuiltIn),
    User(String),
}

/// Built-in annotations the precompile writes on every entity. Names
/// match `arkiv_entitydb`'s `ANNOT_*` byte constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltIn {
    /// `$owner` — entity owner address.
    Owner,
    /// `$creator` — original creator address (immutable).
    Creator,
    /// `$key` — full 32-byte entity key.
    Key,
    /// `$expiration` — block number at which the entity expires.
    Expiration,
    /// `$contentType` — content-type bytes (e.g. `text/plain`).
    ContentType,
    /// `$createdAtBlock` — block number the entity was created at.
    CreatedAtBlock,
}

impl BuiltIn {
    /// The byte-key `arkiv_entitydb` uses on the pair address for this
    /// built-in. Wire-stable: any change here must change the create /
    /// transfer / extend handlers in lockstep.
    pub fn pair_key(self) -> &'static [u8] {
        match self {
            BuiltIn::Owner => crate::ANNOT_OWNER,
            BuiltIn::Creator => crate::ANNOT_CREATOR,
            BuiltIn::Key => crate::ANNOT_KEY,
            BuiltIn::Expiration => crate::ANNOT_EXPIRATION,
            BuiltIn::ContentType => crate::ANNOT_CONTENT_TYPE,
            BuiltIn::CreatedAtBlock => crate::ANNOT_CREATED_AT_BLOCK,
        }
    }
}

impl AnnotKey {
    /// Pair-account key bytes for this annotation.
    pub fn pair_key_bytes(&self) -> &[u8] {
        match self {
            AnnotKey::BuiltIn(b) => b.pair_key(),
            AnnotKey::User(s) => s.as_bytes(),
        }
    }
}

/// Pre-encoded pair-account value. Encoded by [`encode_for_key`] at
/// parse time so the evaluator can call `read_pair_bitmap(state,
/// key_bytes, value.0)` with no further conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotVal(pub Vec<u8>);

/// Parse a query string. Convenience over `tokenize` + `Parser::new`.
pub fn parse(input: &str) -> Result<Query> {
    let tokens = tokenize(input)?;
    let mut p = Parser::new(tokens);
    let q = p.parse_top_level()?;
    Ok(q)
}

// ── Parser machinery ──────────────────────────────────────────────────

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

    /// Consume `expected` or error. Use only on unit-variant tokens —
    /// data-carrying variants need destructuring.
    fn expect(&mut self, expected: &Token) -> Result<()> {
        match self.advance() {
            Some(t) if &t == expected => Ok(()),
            Some(t) => bail!("expected {expected:?}, got {t:?}"),
            None => bail!("expected {expected:?}, got end of input"),
        }
    }

    fn parse_top_level(&mut self) -> Result<Query> {
        // Standalone `*` or `$all` only at top level — not valid inside
        // expressions (those are predicates, not whole-set selectors).
        if matches!(self.peek(), Some(Token::Star | Token::DollarAll)) {
            self.advance();
            if let Some(t) = self.peek() {
                bail!("expected end of input after '*' / '$all', got {t:?}");
            }
            return Ok(Query::All);
        }
        let q = self.parse_or()?;
        if let Some(t) = self.peek() {
            bail!("expected end of input, got {t:?}");
        }
        Ok(q)
    }

    fn parse_or(&mut self) -> Result<Query> {
        let mut q = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.advance();
            let rhs = self.parse_and()?;
            q = Query::Or(Box::new(q), Box::new(rhs));
        }
        Ok(q)
    }

    fn parse_and(&mut self) -> Result<Query> {
        let mut q = self.parse_term()?;
        while matches!(self.peek(), Some(Token::And)) {
            self.advance();
            let rhs = self.parse_term()?;
            q = Query::And(Box::new(q), Box::new(rhs));
        }
        Ok(q)
    }

    fn parse_term(&mut self) -> Result<Query> {
        // `NOT (...)` — require parens after NOT so `NOT a = b` doesn't
        // need to choose between `NOT (a = b)` and `(NOT a) = b`.
        if matches!(self.peek(), Some(Token::Not)) {
            self.advance();
            self.expect(&Token::LParen)?;
            let inner = self.parse_or()?;
            self.expect(&Token::RParen)?;
            return Ok(Query::Not(Box::new(inner)));
        }
        if matches!(self.peek(), Some(Token::LParen)) {
            self.advance();
            let inner = self.parse_or()?;
            self.expect(&Token::RParen)?;
            return Ok(inner);
        }
        self.parse_predicate()
    }

    fn parse_predicate(&mut self) -> Result<Query> {
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
                self.expect(&Token::In)?;
                let values = self.parse_value_list(&key)?;
                Ok(Query::NotIn { key, values })
            }
            Some(Token::In) => {
                self.advance();
                let values = self.parse_value_list(&key)?;
                Ok(Query::In { key, values })
            }
            Some(Token::Gt) => {
                self.advance();
                self.reject_range_on_random_distribution()?;
                let (value, mode) = self.parse_range_value(&key)?;
                Ok(Query::Gt { key, value, mode })
            }
            Some(Token::Gte) => {
                self.advance();
                self.reject_range_on_random_distribution()?;
                let (value, mode) = self.parse_range_value(&key)?;
                Ok(Query::Gte { key, value, mode })
            }
            Some(Token::Lt) => {
                self.advance();
                self.reject_range_on_random_distribution()?;
                let (value, mode) = self.parse_range_value(&key)?;
                Ok(Query::Lt { key, value, mode })
            }
            Some(Token::Lte) => {
                self.advance();
                self.reject_range_on_random_distribution()?;
                let (value, mode) = self.parse_range_value(&key)?;
                Ok(Query::Lte { key, value, mode })
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
            Some(t) => bail!(
                "expected '=', '!=', '>', '>=', '<', '<=', '~', '!~', 'IN', or 'NOT IN' after key; got {t:?}"
            ),
            None => bail!("expected operator after key, got end of input"),
        }
    }

    /// Reject range operators (`>`, `>=`, `<`, `<=`) when the RHS
    /// literal is an `Address` or `EntityKey` — those are
    /// random-distribution values with no meaningful ordering, so the
    /// query would just hit an (always-absent) tier-2 index tree and
    /// return empty. Failing fast at parse time gives a useful error
    /// instead. Equality (`=`, `!=`) and set membership (`IN`,
    /// `NOT IN`) on addresses / keys remain valid — those are backed
    /// by the tier-1 pair bitmap.
    ///
    /// Glob (`~`, `!~`) is already restricted to `Literal::String` by
    /// [`parse_glob_pattern`], so no check is needed for it here.
    ///
    /// Known gap: `$key > "0x…64hex"` (quoted string) is not caught
    /// because the lexer emits `Token::StringLit`, not
    /// `Token::EntityKey`. The runtime falls back to the natural-empty
    /// behavior in that case. The SDK does not currently issue range
    /// queries on `$key`, so this gap is acceptable.
    fn reject_range_on_random_distribution(&self) -> Result<()> {
        match self.peek() {
            Some(Token::Address(_)) => bail!(
                "range operators (>, >=, <, <=) are not supported on \
                 address values — addresses have no meaningful ordering"
            ),
            Some(Token::EntityKey(_)) => bail!(
                "range operators (>, >=, <, <=) are not supported on \
                 entity-key values — entity keys have no meaningful ordering"
            ),
            _ => Ok(()),
        }
    }

    /// Parse a glob pattern: a string literal that must end with `*`.
    /// Strips the trailing `*` and returns the prefix bytes in an
    /// [`AnnotVal`]. Only `$contentType` and user-string keys support
    /// glob.
    fn parse_glob_pattern(&mut self, key: &AnnotKey) -> Result<AnnotVal> {
        match key {
            AnnotKey::BuiltIn(BuiltIn::ContentType) | AnnotKey::User(_) => {}
            AnnotKey::BuiltIn(other) => {
                bail!(
                    "glob ('~' / '!~') is only supported on string-valued keys, \
                     not on {other:?}"
                );
            }
        }
        let lit = self.parse_literal()?;
        let Literal::String(s) = lit else {
            bail!("glob pattern must be a string literal ending in '*'");
        };
        let Some(prefix) = s.strip_suffix('*') else {
            bail!("glob pattern must end in '*', got {s:?}");
        };
        if prefix.contains('*') {
            bail!("only a single trailing '*' is supported in glob patterns, got {s:?}");
        }
        Ok(AnnotVal(prefix.as_bytes().to_vec()))
    }

    fn parse_annot_key(&mut self) -> Result<AnnotKey> {
        match self.advance() {
            Some(Token::DollarOwner) => Ok(AnnotKey::BuiltIn(BuiltIn::Owner)),
            Some(Token::DollarCreator) => Ok(AnnotKey::BuiltIn(BuiltIn::Creator)),
            Some(Token::DollarKey) => Ok(AnnotKey::BuiltIn(BuiltIn::Key)),
            Some(Token::DollarExpiration) => Ok(AnnotKey::BuiltIn(BuiltIn::Expiration)),
            Some(Token::DollarContentType) => Ok(AnnotKey::BuiltIn(BuiltIn::ContentType)),
            Some(Token::DollarCreatedAtBlock) => Ok(AnnotKey::BuiltIn(BuiltIn::CreatedAtBlock)),
            Some(Token::Ident(s)) => Ok(AnnotKey::User(s)),
            Some(t) => bail!("expected annotation key, got {t:?}"),
            None => bail!("expected annotation key, got end of input"),
        }
    }

    fn parse_value(&mut self, key: &AnnotKey) -> Result<AnnotVal> {
        let lit = self.parse_literal()?;
        encode_for_key(key, lit)
    }

    fn parse_range_value(&mut self, key: &AnnotKey) -> Result<(AnnotVal, AnnotMode)> {
        let lit = self.parse_literal()?;
        let mode = mode_for_key_lit(key, &lit);
        Ok((encode_for_key(key, lit)?, mode))
    }

    fn parse_value_list(&mut self, key: &AnnotKey) -> Result<Vec<AnnotVal>> {
        self.expect(&Token::LParen)?;
        let mut vals = Vec::new();
        while !matches!(self.peek(), Some(Token::RParen)) {
            vals.push(self.parse_value(key)?);
        }
        self.expect(&Token::RParen)?;
        if vals.is_empty() {
            bail!("IN / NOT IN value list must be non-empty");
        }
        Ok(vals)
    }

    fn parse_literal(&mut self) -> Result<Literal> {
        match self.advance() {
            Some(Token::Number(n)) => Ok(Literal::Number(n)),
            Some(Token::StringLit(s)) => Ok(Literal::String(s)),
            Some(Token::Address(a)) => Ok(Literal::Address(a)),
            Some(Token::EntityKey(b)) => Ok(Literal::EntityKey(b)),
            Some(t) => bail!("expected literal value, got {t:?}"),
            None => bail!("expected literal value, got end of input"),
        }
    }
}

/// Parser-internal wrapper for literal tokens that survive past the
/// lexer — used by [`encode_for_key`] to pick the right pair-account
/// value layout per built-in.
#[derive(Debug, Clone)]
enum Literal {
    Number(u64),
    String(String),
    Address([u8; 20]),
    EntityKey([u8; 32]),
}

/// Encode a literal as the bytes the pair-account address for `key`
/// uses. Must stay in lockstep with the write-side encoders in
/// `arkiv_entitydb`'s op handlers.
///
/// Per-built-in rules:
///
/// - `$owner` / `$creator`: address literal → 20 raw bytes.
/// - `$key`: entity-key literal → 32 raw bytes.
/// - `$expiration` / `$createdAtBlock`: number → 8-byte BE
///   (matches `encode_u64_be` in the create handler).
/// - `$contentType`: string literal → raw bytes.
///
/// User idents:
///
/// - number → 32-byte BE (matches `encode_u256_be`).
/// - string → raw bytes.
/// - address → 20 raw bytes.
/// Determine the Tier-2 index mode for a (key, literal) pair at parse
/// time, so the interpreter knows which index account to query.
fn mode_for_key_lit(key: &AnnotKey, lit: &Literal) -> AnnotMode {
    match (key, lit) {
        (AnnotKey::BuiltIn(BuiltIn::ContentType), _) => AnnotMode::Str,
        (AnnotKey::User(_), Literal::String(_)) => AnnotMode::Str,
        _ => AnnotMode::Int,
    }
}

/// - entity-key → 32 raw bytes (no zero-strip — matches the
///   precompile's `ATTR_ENTITY_KEY` handling).
fn encode_for_key(key: &AnnotKey, lit: Literal) -> Result<AnnotVal> {
    Ok(AnnotVal(match (key, lit) {
        (AnnotKey::BuiltIn(BuiltIn::Owner | BuiltIn::Creator), Literal::Address(a)) => a.to_vec(),
        (AnnotKey::BuiltIn(b @ (BuiltIn::Owner | BuiltIn::Creator)), other) => {
            bail!("expected address literal for {b:?}, got {other:?}")
        }

        (AnnotKey::BuiltIn(BuiltIn::Key), Literal::EntityKey(b)) => b.to_vec(),
        // The JS SDK quotes all `eq(...)` string values uniformly, so
        // `eq("$key", "0x…64hex")` arrives as a string literal. Accept
        // that shape too, as long as it decodes to exactly 32 bytes.
        (AnnotKey::BuiltIn(BuiltIn::Key), Literal::String(s)) => decode_entity_key_string(&s)?,
        (AnnotKey::BuiltIn(BuiltIn::Key), other) => {
            bail!("expected entity-key literal for $key, got {other:?}")
        }

        (AnnotKey::BuiltIn(BuiltIn::Expiration | BuiltIn::CreatedAtBlock), Literal::Number(n)) => {
            n.to_be_bytes().to_vec()
        }
        (AnnotKey::BuiltIn(b @ (BuiltIn::Expiration | BuiltIn::CreatedAtBlock)), other) => {
            bail!("expected number for {b:?}, got {other:?}")
        }

        (AnnotKey::BuiltIn(BuiltIn::ContentType), Literal::String(s)) => s.into_bytes(),
        (AnnotKey::BuiltIn(BuiltIn::ContentType), other) => {
            bail!("expected string for $contentType, got {other:?}")
        }

        (AnnotKey::User(_), Literal::Number(n)) => {
            // 32-byte BE — matches `encode_u256_be` in arkiv-entitydb.
            let mut buf = [0u8; 32];
            buf[24..].copy_from_slice(&n.to_be_bytes());
            buf.to_vec()
        }
        (AnnotKey::User(_), Literal::String(s)) => s.into_bytes(),
        (AnnotKey::User(_), Literal::Address(a)) => a.to_vec(),
        (AnnotKey::User(_), Literal::EntityKey(b)) => b.to_vec(),
    }))
}

/// Decode a quoted entity-key literal (e.g. `"0x" + 64 hex`) into 32
/// raw bytes. Used to accept the JS SDK's `eq("$key", "0x…")` shape,
/// which arrives as `Literal::String` rather than `Literal::EntityKey`
/// because the SDK uniformly quotes string values in `eq(...)`.
fn decode_entity_key_string(s: &str) -> Result<Vec<u8>> {
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(|| eyre::eyre!("expected 0x-prefixed entity-key string for $key, got {s:?}"))?;
    if stripped.len() != 64 {
        bail!(
            "expected 64 hex chars for $key, got {} ({s:?})",
            stripped.len()
        );
    }
    alloy_primitives::hex::decode(stripped)
        .map_err(|e| eyre::eyre!("invalid hex in $key literal {s:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn p(s: &str) -> Query {
        parse(s).unwrap_or_else(|e| panic!("parse {s:?}: {e}"))
    }

    fn user(k: &str) -> AnnotKey {
        AnnotKey::User(k.into())
    }
    fn builtin(b: BuiltIn) -> AnnotKey {
        AnnotKey::BuiltIn(b)
    }
    fn val(bytes: &[u8]) -> AnnotVal {
        AnnotVal(bytes.to_vec())
    }
    fn user_num(n: u64) -> AnnotVal {
        let mut buf = [0u8; 32];
        buf[24..].copy_from_slice(&n.to_be_bytes());
        AnnotVal(buf.to_vec())
    }

    // ── Top-level ────────────────────────────────────────────────────

    #[test]
    fn star_and_dollar_all() {
        assert_eq!(p("*"), Query::All);
        assert_eq!(p("$all"), Query::All);
        assert_eq!(p("  *   "), Query::All);
    }

    #[test]
    fn top_level_trailing_garbage_errors() {
        assert!(parse("* foo").is_err());
        assert!(parse("$all = 1").is_err());
    }

    // ── Equality / inequality ───────────────────────────────────────

    #[test]
    fn eq_user_string() {
        assert_eq!(
            p(r#"tag = "music""#),
            Query::Eq {
                key: user("tag"),
                value: val(b"music")
            },
        );
    }

    #[test]
    fn eq_user_numeric_is_32_bytes() {
        assert_eq!(
            p("score = 42"),
            Query::Eq {
                key: user("score"),
                value: user_num(42)
            }
        );
    }

    #[test]
    fn eq_builtin_owner_address() {
        let addr = [
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        ];
        assert_eq!(
            p("$owner = 0xaabbccddeeff00112233445566778899aabbccdd"),
            Query::Eq {
                key: builtin(BuiltIn::Owner),
                value: val(&addr)
            },
        );
    }

    #[test]
    fn eq_builtin_expiration_is_8_bytes_be() {
        assert_eq!(
            p("$expiration = 100"),
            Query::Eq {
                key: builtin(BuiltIn::Expiration),
                value: val(&100u64.to_be_bytes()),
            },
        );
    }

    #[test]
    fn eq_builtin_key_is_32_bytes() {
        let mut k = [0u8; 32];
        k[..20].copy_from_slice(&[0x11; 20]);
        assert_eq!(
            p("$key = 0x1111111111111111111111111111111111111111000000000000000000000000"),
            Query::Eq {
                key: builtin(BuiltIn::Key),
                value: val(&k)
            },
        );
    }

    #[test]
    fn eq_builtin_key_accepts_quoted_hex() {
        // The JS SDK's `eq("$key", entityKey)` produces a quoted
        // string literal — `$key = "0x…"`. Decode it to the same 32
        // bytes as the unquoted form.
        let mut k = [0u8; 32];
        k[..20].copy_from_slice(&[0x22; 20]);
        let unquoted =
            p("$key = 0x2222222222222222222222222222222222222222000000000000000000000000");
        let quoted =
            p(r#"$key = "0x2222222222222222222222222222222222222222000000000000000000000000""#);
        assert_eq!(quoted, unquoted);
        assert_eq!(
            quoted,
            Query::Eq {
                key: builtin(BuiltIn::Key),
                value: val(&k)
            }
        );
    }

    #[test]
    fn eq_builtin_key_rejects_malformed_quoted_hex() {
        // Wrong length
        assert!(parse(r#"$key = "0x1234""#).is_err());
        // Missing 0x prefix
        let sixty_four_hex_no_prefix = "a".repeat(64);
        assert!(parse(&format!(r#"$key = "{sixty_four_hex_no_prefix}""#)).is_err());
        // Non-hex chars
        let bad = format!("0x{}", "z".repeat(64));
        assert!(parse(&format!(r#"$key = "{bad}""#)).is_err());
    }

    #[test]
    fn eq_builtin_content_type_is_raw_bytes() {
        assert_eq!(
            p(r#"$contentType = "text/plain""#),
            Query::Eq {
                key: builtin(BuiltIn::ContentType),
                value: val(b"text/plain")
            },
        );
    }

    #[test]
    fn neq() {
        assert_eq!(
            p(r#"tag != "music""#),
            Query::Neq {
                key: user("tag"),
                value: val(b"music")
            },
        );
    }

    #[test]
    fn type_mismatch_errors() {
        assert!(parse(r#"$expiration = "foo""#).is_err());
        assert!(parse("$owner = 42").is_err());
        assert!(parse(r#"$contentType = 0xaabbccddeeff00112233445566778899aabbccdd"#).is_err());
    }

    // ── Range ────────────────────────────────────────────────────────

    #[test]
    fn range_user_numeric() {
        assert_eq!(
            p("score > 100"),
            Query::Gt {
                key: user("score"),
                value: user_num(100),
                mode: AnnotMode::Int,
            }
        );
        assert_eq!(
            p("score <= 50"),
            Query::Lte {
                key: user("score"),
                value: user_num(50),
                mode: AnnotMode::Int,
            }
        );
    }

    #[test]
    fn range_builtin_numeric() {
        assert_eq!(
            p("$expiration >= 1000000"),
            Query::Gte {
                key: builtin(BuiltIn::Expiration),
                value: val(&1_000_000u64.to_be_bytes()),
                mode: AnnotMode::Int,
            }
        );
    }

    #[test]
    fn range_rejects_address_literal() {
        // Addresses have no meaningful ordering — range operators are
        // rejected at parse time for both built-ins and user attrs.
        let addr = "0xaabbccddeeff00112233445566778899aabbccdd";
        for op in [">", ">=", "<", "<="] {
            assert!(
                parse(&format!("$owner {op} {addr}")).is_err(),
                "range op {op:?} on address literal (built-in) should error"
            );
            assert!(
                parse(&format!("ref {op} {addr}")).is_err(),
                "range op {op:?} on address literal (user attr) should error"
            );
        }
    }

    #[test]
    fn range_rejects_entity_key_literal() {
        // Entity keys are random 32-byte hashes — same rationale as
        // addresses.
        let ek = "0x1111111111111111111111111111111111111111000000000000000000000000";
        for op in [">", ">=", "<", "<="] {
            assert!(
                parse(&format!("$key {op} {ek}")).is_err(),
                "range op {op:?} on entity-key literal (built-in) should error"
            );
            assert!(
                parse(&format!("ref {op} {ek}")).is_err(),
                "range op {op:?} on entity-key literal (user attr) should error"
            );
        }
    }

    #[test]
    fn eq_and_in_still_accept_address_and_entity_key() {
        // Sanity: the range-only rejection doesn't leak into the
        // equality / set-membership paths — those are backed by the
        // tier-1 pair bitmap and remain valid for random-distribution
        // values.
        let addr = "0xaabbccddeeff00112233445566778899aabbccdd";
        let ek = "0x1111111111111111111111111111111111111111000000000000000000000000";
        assert!(parse(&format!("$owner = {addr}")).is_ok());
        assert!(parse(&format!("$owner != {addr}")).is_ok());
        assert!(parse(&format!("$owner IN ({addr})")).is_ok());
        assert!(parse(&format!("$key = {ek}")).is_ok());
        assert!(parse(&format!("$key IN ({ek})")).is_ok());
    }

    // ── Inclusion ────────────────────────────────────────────────────

    #[test]
    fn inclusion() {
        let a1 = [0x11u8; 20];
        let a2 = [0x22u8; 20];
        assert_eq!(
            p("$owner IN \
                 (0x1111111111111111111111111111111111111111 \
                  0x2222222222222222222222222222222222222222)",),
            Query::In {
                key: builtin(BuiltIn::Owner),
                values: vec![val(&a1), val(&a2)],
            },
        );
    }

    #[test]
    fn not_inclusion() {
        let a1 = [0x11u8; 20];
        assert_eq!(
            p("$owner NOT IN (0x1111111111111111111111111111111111111111)"),
            Query::NotIn {
                key: builtin(BuiltIn::Owner),
                values: vec![val(&a1)],
            },
        );
    }

    #[test]
    fn empty_inclusion_list_errors() {
        assert!(parse("$owner IN ()").is_err());
    }

    // ── Boolean ops ──────────────────────────────────────────────────

    #[test]
    fn and_chains() {
        let q =
            p(r#"tag = "a" && score = 1 && $owner != 0x1111111111111111111111111111111111111111"#);
        // Left-associative: ((tag=a && score=1) && $owner!=...)
        match q {
            Query::And(left, right) => {
                assert!(matches!(*left, Query::And(_, _)));
                assert!(matches!(*right, Query::Neq { .. }));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn or_chains() {
        let q = p(r#"tag = "a" || tag = "b" || tag = "c""#);
        match q {
            Query::Or(left, right) => {
                assert!(matches!(*left, Query::Or(_, _)));
                assert!(matches!(*right, Query::Eq { .. }));
            }
            other => panic!("expected Or, got {other:?}"),
        }
    }

    #[test]
    fn precedence_and_over_or() {
        // `a OR b AND c` should parse as `a OR (b AND c)`.
        let q = p(r#"tag = "a" || tag = "b" && tag = "c""#);
        match q {
            Query::Or(left, right) => {
                assert!(matches!(*left, Query::Eq { .. }));
                assert!(matches!(*right, Query::And(_, _)));
            }
            other => panic!("expected Or with And on right, got {other:?}"),
        }
    }

    #[test]
    fn parens_override_precedence() {
        let q = p(r#"(tag = "a" || tag = "b") && tag = "c""#);
        match q {
            Query::And(left, right) => {
                assert!(matches!(*left, Query::Or(_, _)));
                assert!(matches!(*right, Query::Eq { .. }));
            }
            other => panic!("expected And with Or on left, got {other:?}"),
        }
    }

    // ── NOT ──────────────────────────────────────────────────────────

    #[test]
    fn not_around_paren() {
        let q = p(r#"NOT (tag = "x")"#);
        match q {
            Query::Not(inner) => assert!(matches!(*inner, Query::Eq { .. })),
            other => panic!("expected Not, got {other:?}"),
        }
    }

    #[test]
    fn bang_synonym_for_not() {
        let q = p(r#"!(tag = "x")"#);
        assert!(matches!(q, Query::Not(_)));
    }

    #[test]
    fn not_requires_parens() {
        // `NOT tag = "x"` without parens is rejected.
        assert!(parse(r#"NOT tag = "x""#).is_err());
    }

    // ── Keyword aliases ──────────────────────────────────────────────

    #[test]
    fn keyword_aliases() {
        let canonical = p(r#"tag = "a" && score = 1"#);
        assert_eq!(p(r#"tag = "a" AND score = 1"#), canonical);
        assert_eq!(p(r#"tag = "a" and score = 1"#), canonical);

        let or_canonical = p(r#"tag = "a" || tag = "b""#);
        assert_eq!(p(r#"tag = "a" OR tag = "b""#), or_canonical);
        assert_eq!(p(r#"tag = "a" or tag = "b""#), or_canonical);
    }

    // ── Error surface ────────────────────────────────────────────────

    #[test]
    fn missing_value_errors() {
        assert!(parse("tag =").is_err());
        assert!(parse("tag = AND").is_err());
    }

    #[test]
    fn missing_operator_errors() {
        assert!(parse(r#"tag "x""#).is_err());
    }

    #[test]
    fn unbalanced_parens_error() {
        assert!(parse(r#"(tag = "x""#).is_err());
        assert!(parse(r#"tag = "x")"#).is_err());
    }
}
