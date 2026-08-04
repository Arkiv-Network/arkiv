//! Recursive-descent parser: tokens → [`Query`].
//!
//! ```text
//! top        → '*' | expr
//! expr       → andExpr { OR andExpr }
//! andExpr    → unary { AND unary }
//! unary      → NOT unary | primary
//! primary    → '(' expr ')' | predicate
//! predicate  → attrRef compOp value | attrRef STARTSWITH strValue
//! ```
//!
//! Precedence, tightest first: `NOT`, `AND`, `OR`.
//!
//! Two jobs beyond shape. First, **typing**: a value carries its own type, and
//! this is where a value is checked against the attribute it is compared to
//! (`$owner` takes an address) and against its operator (only the numeric types
//! are ordered). A range operator on an equality-only type is an error here, not
//! an empty result later — the spec is explicit that the two must not be
//! confused. Second, **limits**: length, predicate count and nesting depth are
//! bounded, because `parse` runs on unauthenticated RPC input.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use arkiv_interfaces::entity::{AttributeType, AttributeValue};
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn, Query};

use crate::error::{ParseError, ParseErrorKind};
use crate::lexer::{SpannedToken, Token, TypeTag, tokenize};
use crate::limits;
use crate::literal;

/// Parse a query string into a [`Query`] AST. See the [module docs](self).
pub fn parse(input: &str) -> Result<Query, ParseError> {
    if input.len() > limits::MAX_QUERY_BYTES {
        return Err(ParseError::whole(
            ParseErrorKind::Limit,
            "query is too long",
        ));
    }
    let tokens = tokenize(input)?;
    Parser::new(tokens, input.len()).parse_top_level()
}

struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
    /// Byte offset one past the input, for "unexpected end of query" errors.
    end: usize,
    depth: usize,
    predicates: usize,
}

impl Parser {
    fn new(tokens: Vec<SpannedToken>, end: usize) -> Self {
        Self {
            tokens,
            pos: 0,
            end,
            depth: 0,
            predicates: 0,
        }
    }

    fn peek(&self) -> Option<&SpannedToken> {
        self.tokens.get(self.pos)
    }

    fn peek_token(&self) -> Option<&Token> {
        self.peek().map(|spanned| &spanned.token)
    }

    fn advance(&mut self) -> Option<SpannedToken> {
        let spanned = self.tokens.get(self.pos).cloned()?;
        self.pos += 1;
        Some(spanned)
    }

    /// Where the next token starts, or the end of input.
    fn next_position(&self) -> usize {
        self.peek().map_or(self.end, |spanned| spanned.start)
    }

    fn parse_top_level(&mut self) -> Result<Query, ParseError> {
        if self.tokens.is_empty() {
            return Err(ParseError::syntax(
                0,
                "empty query — write a predicate, or * to match every entity",
            ));
        }
        if matches!(self.peek_token(), Some(Token::Star)) {
            self.advance();
            if let Some(spanned) = self.peek() {
                return Err(ParseError::syntax(
                    spanned.start,
                    "* matches every entity and cannot be combined with other predicates",
                ));
            }
            return Ok(Query::All);
        }
        let query = self.parse_expr()?;
        if let Some(spanned) = self.peek() {
            return Err(ParseError::syntax(
                spanned.start,
                "unexpected trailing input — did you mean to join these with AND or OR?",
            ));
        }
        Ok(query)
    }

    fn parse_expr(&mut self) -> Result<Query, ParseError> {
        let mut query = self.parse_and()?;
        while matches!(self.peek_token(), Some(Token::Or)) {
            self.advance();
            let right = self.parse_and()?;
            query = Query::Or(Box::new(query), Box::new(right));
        }
        Ok(query)
    }

    fn parse_and(&mut self) -> Result<Query, ParseError> {
        let mut query = self.parse_unary()?;
        while matches!(self.peek_token(), Some(Token::And)) {
            self.advance();
            let right = self.parse_unary()?;
            query = Query::And(Box::new(query), Box::new(right));
        }
        Ok(query)
    }

    fn parse_unary(&mut self) -> Result<Query, ParseError> {
        let Some(spanned) = self.peek() else {
            return Err(ParseError::syntax(self.end, "expected a predicate"));
        };
        if spanned.token != Token::Not {
            return self.parse_primary();
        }
        let start = spanned.start;
        self.advance();
        self.descend(start)?;
        // NOT binds tighter than AND, so it takes the next unary, not the
        // whole conjunction: `NOT a = true AND b = true` is `(NOT a) AND b`.
        let inner = self.parse_unary()?;
        self.depth -= 1;
        Ok(Query::Not(Box::new(inner)))
    }

    fn parse_primary(&mut self) -> Result<Query, ParseError> {
        let Some(spanned) = self.peek() else {
            return Err(ParseError::syntax(self.end, "expected a predicate"));
        };
        if spanned.token != Token::LParen {
            return self.parse_predicate();
        }
        let start = spanned.start;
        self.advance();
        self.descend(start)?;
        let inner = self.parse_expr()?;
        match self.advance() {
            Some(SpannedToken {
                token: Token::RParen,
                ..
            }) => {}
            _ => {
                return Err(ParseError::syntax(start, "unclosed group — expected ')'"));
            }
        }
        self.depth -= 1;
        Ok(inner)
    }

    /// Enter one level of nesting, enforcing the depth bound.
    fn descend(&mut self, position: usize) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > limits::MAX_NESTING_DEPTH {
            return Err(ParseError::at(
                position,
                ParseErrorKind::Limit,
                "query is nested too deeply",
            ));
        }
        Ok(())
    }

    fn parse_predicate(&mut self) -> Result<Query, ParseError> {
        self.predicates += 1;
        if self.predicates > limits::MAX_PREDICATES {
            return Err(ParseError::at(
                self.next_position(),
                ParseErrorKind::Limit,
                "query has too many predicates",
            ));
        }

        let key = self.parse_attr_ref()?;
        let Some(operator) = self.advance() else {
            return Err(ParseError::syntax(
                self.end,
                "expected a comparison operator (= < <= > >= STARTSWITH) after the attribute",
            ));
        };

        match operator.token {
            Token::Eq | Token::Lt | Token::Lte | Token::Gt | Token::Gte => {
                let (value, position) = self.parse_value(&key)?;
                check_operator(&operator.token, &value, position)?;
                Ok(comparison(&operator.token, key, value))
            }
            Token::StartsWith => {
                let (value, position) = self.parse_value(&key)?;
                if !matches!(value, AttributeValue::Str(_)) {
                    return Err(ParseError::type_error(
                        position,
                        "STARTSWITH matches a string prefix — write str('…')",
                    ));
                }
                Ok(Query::StartsWith { key, value })
            }
            Token::Neq => Err(ParseError::type_error(
                operator.start,
                "!= is not part of the query language — write NOT (attr = value) for the complement",
            )),
            _ => Err(ParseError::syntax(
                operator.start,
                "expected a comparison operator (= < <= > >= STARTSWITH)",
            )),
        }
    }

    fn parse_attr_ref(&mut self) -> Result<AnnotKey, ParseError> {
        let Some(spanned) = self.advance() else {
            return Err(ParseError::syntax(self.end, "expected an attribute name"));
        };
        match spanned.token {
            Token::Name(name) => {
                validate_user_name(&name, spanned.start)?;
                Ok(AnnotKey::User(name))
            }
            Token::SysName(name) => builtin_from_name(&name, spanned.start).map(AnnotKey::BuiltIn),
            Token::Exists => Err(ParseError::type_error(
                spanned.start,
                "exists(…) is not supported — the index has no per-attribute presence set in this version",
            )),
            Token::TypeOf => Err(ParseError::type_error(
                spanned.start,
                "typeof(…) is not supported — a value predicate already asserts the attribute's type",
            )),
            Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::StartsWith => Err(ParseError::syntax(
                spanned.start,
                "a reserved word cannot be used as an attribute name",
            )),
            _ => Err(ParseError::syntax(
                spanned.start,
                "expected an attribute name",
            )),
        }
    }

    /// Read the value a predicate compares against, and check it suits the
    /// attribute. Returns the value and where it started.
    fn parse_value(&mut self, key: &AnnotKey) -> Result<(AnnotVal, usize), ParseError> {
        let Some(spanned) = self.advance() else {
            return Err(ParseError::syntax(
                self.end,
                "expected a value, e.g. i32(10) or str('Bob')",
            ));
        };
        let position = spanned.start;
        let value = match spanned.token {
            Token::Tagged {
                tag,
                body,
                body_start,
            } => literal::parse_tagged(tag, &body, body_start)?,
            Token::True => AttributeValue::Bool(true),
            Token::False => AttributeValue::Bool(false),
            Token::Int(text) => bare_int_value(key, &text, position)?,
            Token::Str(content) => bare_str_value(key, content, position)?,
            _ => {
                return Err(ParseError::syntax(
                    position,
                    "expected a value, e.g. i32(10) or str('Bob')",
                ));
            }
        };
        check_value_type(key, &value, position)?;
        let value = normalize_builtin_value(key, value);
        Ok((value, position))
    }
}

/// Build the comparison node for an already-validated operator.
fn comparison(operator: &Token, key: AnnotKey, value: AnnotVal) -> Query {
    match operator {
        Token::Eq => Query::Eq { key, value },
        Token::Lt => Query::Lt { key, value },
        Token::Lte => Query::Lte { key, value },
        Token::Gt => Query::Gt { key, value },
        Token::Gte => Query::Gte { key, value },
        // `parse_predicate` only reaches here with the five above.
        _ => unreachable!("comparison called with a non-comparison operator"),
    }
}

/// An untagged number literal.
///
/// It means `i32`, and only `i32` — every other numeric type must be tagged.
/// That keeps the exact-type assertion visible at the call site: a predicate
/// asserts "this attribute exists *with this type*", so a bare number silently
/// meaning different things in different places would make `level >= 10` a
/// question you cannot answer by reading it.
///
/// The consequence worth knowing: the system block heights are `u64`, so they
/// need the tag (`$expiresAt < u64(1200000)`) even though they are obviously
/// numbers.
fn bare_int_value(key: &AnnotKey, text: &str, position: usize) -> Result<AnnotVal, ParseError> {
    if let AnnotKey::BuiltIn(field) = key {
        let expected = builtin_type(*field);
        return Err(ParseError::type_error(
            position,
            if expected == AttributeType::U64 {
                alloc::format!(
                    "this system attribute holds u64 — write u64({})",
                    text.trim()
                )
            } else {
                alloc::format!(
                    "this system attribute holds {}, but the value is an untagged number \
                     (which means i32)",
                    expected.name(),
                )
            },
        ));
    }
    literal::parse_bare_i32(text, position)
}

/// An untagged string: system attributes only. `$key`, `$owner` and `$creator`
/// accept their hex forms quoted, which is how the JS SDK spells them.
fn bare_str_value(
    key: &AnnotKey,
    content: String,
    position: usize,
) -> Result<AnnotVal, ParseError> {
    match key {
        AnnotKey::BuiltIn(BuiltIn::ContentType) => {
            literal::validate_str_len(&content, position)?;
            Ok(AttributeValue::Str(content))
        }
        AnnotKey::BuiltIn(BuiltIn::Owner | BuiltIn::Creator) => {
            literal::parse_addr(&content, position).map(AttributeValue::EthereumAddress)
        }
        AnnotKey::BuiltIn(BuiltIn::Key) => {
            literal::parse_word_hex(&content, position, "key").map(AttributeValue::EntityKey)
        }
        AnnotKey::BuiltIn(_) => Err(ParseError::type_error(
            position,
            "this system attribute is not a string",
        )),
        AnnotKey::User(_) => Err(ParseError::type_error(
            position,
            "untagged strings are only valid for system attributes — write str('…')",
        )),
    }
}

/// The type a system attribute is fixed to carry.
/// A system attribute's type **as the client writes it**.
///
/// For the block heights this is `u64`, which is what the spec's tag table says
/// and what a query must spell. It is deliberately not the same thing as how
/// the value is keyed in the index — see [`normalize_builtin_value`].
fn builtin_type(field: BuiltIn) -> AttributeType {
    match field {
        BuiltIn::Owner | BuiltIn::Creator => AttributeType::EthereumAddress,
        BuiltIn::Key => AttributeType::EntityKey,
        BuiltIn::ExpiresAt | BuiltIn::CreatedAt => AttributeType::U64,
        BuiltIn::ContentType => AttributeType::Str,
    }
}

/// Re-encode a system value from its surface type to the one the index is keyed
/// on.
///
/// The block heights are `u64` to a client but are recorded as right-aligned
/// `u256` words (`annotation::entity_annotations`), so a `u64(…)` literal has to
/// become the same word the writer stored or it would hash to a different
/// bucket and match nothing. This is the one place the surface and the index
/// disagree, and it is confined here on purpose — `arkiv-engine.md` §2 allows
/// internal representations to diverge from the wire.
fn normalize_builtin_value(key: &AnnotKey, value: AnnotVal) -> AnnotVal {
    match (key, &value) {
        (
            AnnotKey::BuiltIn(BuiltIn::ExpiresAt | BuiltIn::CreatedAt),
            AttributeValue::U64(height),
        ) => AttributeValue::u256_from_u64(*height),
        _ => value,
    }
}

/// A system attribute's type is protocol-fixed, so a mismatched literal is an
/// error rather than a query that silently matches nothing.
fn check_value_type(key: &AnnotKey, value: &AnnotVal, position: usize) -> Result<(), ParseError> {
    let AnnotKey::BuiltIn(field) = key else {
        return Ok(());
    };
    let expected = builtin_type(*field);
    if value.attr_type() == expected {
        return Ok(());
    }
    Err(ParseError::type_error(
        position,
        alloc::format!(
            "this system attribute holds {}, but the value is {}",
            expected.name(),
            value.attr_type().name(),
        ),
    ))
}

/// The operator × type matrix: only the numeric types are ordered.
fn check_operator(operator: &Token, value: &AnnotVal, position: usize) -> Result<(), ParseError> {
    let ordered = matches!(
        value.attr_type(),
        AttributeType::Int | AttributeType::U64 | AttributeType::U256 | AttributeType::Decimal
    );
    let is_range = matches!(operator, Token::Lt | Token::Lte | Token::Gt | Token::Gte);
    if is_range && !ordered {
        return Err(ParseError::type_error(
            position,
            alloc::format!(
                "{} values have no ordering — only i32, u64, u256 and dec support < <= > >=",
                value.attr_type().name(),
            ),
        ));
    }
    Ok(())
}

/// User attribute names: `[A-Za-z][A-Za-z0-9_.\-]*`, at most 32 bytes, and never
/// one of the language's own words.
///
/// The charset is the lexer's business; what is left is the length cap and the
/// reserved names. Type tags are only tags in front of a `(`, so a bare `str`
/// arrives here as an ordinary name and has to be rejected by hand.
fn validate_user_name(name: &str, position: usize) -> Result<(), ParseError> {
    if name.len() > limits::MAX_ATTRIBUTE_NAME_BYTES {
        return Err(ParseError::syntax(
            position,
            "attribute names are limited to 32 bytes",
        ));
    }
    if let Some(tag) = TypeTag::from_name(name) {
        return Err(ParseError::syntax(
            position,
            alloc::format!(
                "{} is a type name and cannot be an attribute name",
                tag.name()
            ),
        ));
    }
    Ok(())
}

/// Resolve a `$name` to its built-in field.
///
/// Names are case-sensitive, like every other attribute. Fields the spec lists
/// but this version cannot serve get their own message, so a client can tell
/// "not yet" from "no such thing".
fn builtin_from_name(name: &str, position: usize) -> Result<BuiltIn, ParseError> {
    match name {
        "owner" => Ok(BuiltIn::Owner),
        "creator" => Ok(BuiltIn::Creator),
        "key" => Ok(BuiltIn::Key),
        "expiresAt" => Ok(BuiltIn::ExpiresAt),
        "createdAt" => Ok(BuiltIn::CreatedAt),
        "contentType" => Ok(BuiltIn::ContentType),

        "updatedAt" => Err(ParseError::type_error(
            position,
            "$updatedAt is not queryable — it is returned by projections only",
        )),
        "creationFlags" => Err(ParseError::type_error(
            position,
            "$creationFlags is not queryable — it is returned by projections only",
        )),
        "payload" => Err(ParseError::type_error(
            position,
            "$payload is not queryable — bytes values carry no index",
        )),

        // The pre-spec spellings, named so an old query says what to change.
        "expiration" => Err(ParseError::type_error(
            position,
            "unknown system attribute $expiration — it is now $expiresAt",
        )),
        "createdAtBlock" => Err(ParseError::type_error(
            position,
            "unknown system attribute $createdAtBlock — it is now $createdAt",
        )),

        _ => Err(ParseError::type_error(
            position,
            alloc::format!(
                "unknown system attribute ${name} — expected $key, $owner, $creator, \
                 $expiresAt, $createdAt or $contentType"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::ToString;

    fn user(name: &str) -> AnnotKey {
        AnnotKey::User(name.to_string())
    }

    fn built_in(field: BuiltIn) -> AnnotKey {
        AnnotKey::BuiltIn(field)
    }

    /// The error kind a bad query produces.
    fn kind_of(query: &str) -> ParseErrorKind {
        parse(query).unwrap_err().kind
    }

    fn addr_hex(byte: &str) -> String {
        format!("0x{}", byte.repeat(20))
    }

    fn word_hex(byte: &str) -> String {
        format!("0x{}", byte.repeat(32))
    }

    // ── the typed literals ──────────────────────────────────────────────

    #[test]
    fn every_tag_parses_to_its_own_type() {
        assert_eq!(
            parse("level = i32(10)").unwrap(),
            Query::Eq {
                key: user("level"),
                value: AttributeValue::Int(10),
            }
        );
        assert_eq!(
            parse("balance = u256(1000000)").unwrap(),
            Query::Eq {
                key: user("balance"),
                value: AttributeValue::u256_from_u64(1_000_000),
            }
        );
        assert_eq!(
            parse("name = str('Bob')").unwrap(),
            Query::Eq {
                key: user("name"),
                value: AttributeValue::Str("Bob".into()),
            }
        );
        assert_eq!(
            parse("flagged = true").unwrap(),
            Query::Eq {
                key: user("flagged"),
                value: AttributeValue::Bool(true),
            }
        );
        assert_eq!(
            parse("flagged = false").unwrap(),
            Query::Eq {
                key: user("flagged"),
                value: AttributeValue::Bool(false),
            }
        );
        assert!(matches!(
            parse(&format!("parent = key({})", word_hex("ab"))).unwrap(),
            Query::Eq {
                value: AttributeValue::EntityKey(_),
                ..
            }
        ));
        assert!(matches!(
            parse(&format!("hash = bytes32({})", word_hex("cd"))).unwrap(),
            Query::Eq {
                value: AttributeValue::Bytes32(_),
                ..
            }
        ));
        assert!(matches!(
            parse(&format!("who = addr({})", addr_hex("ab"))).unwrap(),
            Query::Eq {
                value: AttributeValue::EthereumAddress(_),
                ..
            }
        ));
        assert!(matches!(
            parse("score = dec(3.5)").unwrap(),
            Query::Eq {
                value: AttributeValue::Decimal(_),
                ..
            }
        ));
    }

    /// The point of the type system: same name, same digits, different type —
    /// and therefore a different predicate that can never match the other.
    #[test]
    fn the_same_number_under_two_tags_is_two_different_predicates() {
        let as_i32 = parse("level = i32(10)").unwrap();
        let as_u256 = parse("level = u256(10)").unwrap();
        assert_ne!(as_i32, as_u256);
        let (Query::Eq { value: left, .. }, Query::Eq { value: right, .. }) = (&as_i32, &as_u256)
        else {
            panic!("expected two equalities");
        };
        assert_ne!(left.attr_type(), right.attr_type());
        // And their index bytes differ, so they hash to different buckets.
        assert_ne!(left.index_bytes(), right.index_bytes());
    }

    /// An untagged number means `i32` — and *only* `i32`, so it stays readable
    /// as an exact-type assertion. A bare string has no such default.
    #[test]
    fn a_bare_number_is_an_i32() {
        assert_eq!(
            parse("level = 10").unwrap(),
            Query::Eq {
                key: AnnotKey::User("level".into()),
                value: AttributeValue::Int(10),
            }
        );
        // Identical to spelling the tag out.
        assert_eq!(
            parse("level = 10").unwrap(),
            parse("level = i32(10)").unwrap()
        );
        // A bare number never widens to reach a value i32 cannot hold.
        assert_eq!(kind_of("level = 2147483648"), ParseErrorKind::Literal);
        // Bare strings still have no default.
        assert_eq!(kind_of("name = 'Bob'"), ParseErrorKind::Type);
    }

    #[test]
    fn literal_validation_errors_are_their_own_kind() {
        assert_eq!(kind_of("level = i32(2147483648)"), ParseErrorKind::Literal);
        assert_eq!(
            kind_of("score = dec(0.1234567890123456789)"),
            ParseErrorKind::Literal
        );
        assert_eq!(kind_of("who = addr(0xdead)"), ParseErrorKind::Literal);
        assert_eq!(kind_of("parent = key(0xdead)"), ParseErrorKind::Literal);
    }

    // ── the operator × type matrix ──────────────────────────────────────

    #[test]
    fn ranges_are_allowed_on_the_numeric_types() {
        for query in [
            "level > i32(1)",
            "level >= i32(1)",
            "level < i32(1)",
            "level <= i32(1)",
            "balance > u256(1)",
            "score >= dec(3.5)",
        ] {
            assert!(parse(query).is_ok(), "{query}");
        }
    }

    #[test]
    fn ranges_are_rejected_on_the_unordered_types() {
        let unordered = [
            "flagged > true".to_string(),
            "name > str('a')".to_string(),
            format!("who > addr({})", addr_hex("ab")),
            format!("parent > key({})", word_hex("ab")),
            format!("hash > bytes32({})", word_hex("ab")),
        ];
        for query in unordered {
            let err = parse(&query).unwrap_err();
            assert_eq!(err.kind, ParseErrorKind::Type, "{query}");
            assert!(err.message.contains("no ordering"), "{query}: {err}");
        }
    }

    #[test]
    fn startswith_takes_strings_only() {
        assert_eq!(
            parse("desc STARTSWITH str('ab')").unwrap(),
            Query::StartsWith {
                key: user("desc"),
                value: AttributeValue::Str("ab".into()),
            }
        );
        // Case-insensitive, like every keyword.
        assert!(parse("desc startswith str('ab')").is_ok());
        assert_eq!(kind_of("level STARTSWITH i32(1)"), ParseErrorKind::Type);
        assert_eq!(kind_of("flagged STARTSWITH true"), ParseErrorKind::Type);
    }

    // ── system attributes ───────────────────────────────────────────────

    #[test]
    fn system_attributes_resolve_and_type_check() {
        assert_eq!(
            parse(&format!("$owner = addr({})", addr_hex("ab"))).unwrap(),
            Query::Eq {
                key: built_in(BuiltIn::Owner),
                value: AttributeValue::EthereumAddress([0xab; 20]),
            }
        );
        assert!(matches!(
            parse(&format!("$creator = addr({})", addr_hex("cd"))).unwrap(),
            Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Creator),
                ..
            }
        ));
        assert!(matches!(
            parse(&format!("$key = key({})", word_hex("ab"))).unwrap(),
            Query::Eq {
                key: AnnotKey::BuiltIn(BuiltIn::Key),
                ..
            }
        ));
        assert_eq!(
            parse("$contentType = str('text/plain')").unwrap(),
            Query::Eq {
                key: built_in(BuiltIn::ContentType),
                value: AttributeValue::Str("text/plain".into()),
            }
        );
    }

    /// The block heights are `u64`, so they must be tagged — the one place the
    /// "untagged means i32" default is most tempting to rely on and would be
    /// wrong. The tagged value is re-encoded to the word the index is keyed on.
    #[test]
    fn block_heights_need_the_u64_tag_and_range_freely() {
        assert_eq!(
            parse("$expiresAt < u64(1200000)").unwrap(),
            Query::Lt {
                key: built_in(BuiltIn::ExpiresAt),
                // Surface u64, but stored — and therefore queried — as the
                // right-aligned word the writer indexed.
                value: AttributeValue::u256_from_u64(1_200_000),
            }
        );
        assert!(matches!(
            parse("$createdAt >= u64(7)").unwrap(),
            Query::Gte {
                key: AnnotKey::BuiltIn(BuiltIn::CreatedAt),
                ..
            }
        ));

        // Untagged is a type error that names the fix, rather than silently
        // comparing an i32 against a u64 attribute and matching nothing.
        let err = parse("$expiresAt < 1200000").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Type);
        assert!(err.message.contains("u64(1200000)"), "{err}");
    }

    /// A user attribute typed `u64` keeps its own type — only the built-in
    /// block heights are re-encoded.
    #[test]
    fn user_u64_attributes_keep_their_type() {
        assert_eq!(
            parse("height = u64(42)").unwrap(),
            Query::Eq {
                key: AnnotKey::User("height".into()),
                value: AttributeValue::U64(42),
            }
        );
        // And it is range-indexable, like the other numerics.
        assert!(parse("height >= u64(1)").is_ok());
    }

    #[test]
    fn system_attributes_accept_their_quoted_hex_forms() {
        // The JS SDK spells these as strings.
        assert_eq!(
            parse(&format!("$owner = '{}'", addr_hex("ab"))).unwrap(),
            parse(&format!("$owner = addr({})", addr_hex("ab"))).unwrap(),
        );
        assert_eq!(
            parse(&format!("$key = '{}'", word_hex("ab"))).unwrap(),
            parse(&format!("$key = key({})", word_hex("ab"))).unwrap(),
        );
        assert_eq!(
            parse("$contentType = 'text/plain'").unwrap(),
            parse("$contentType = str('text/plain')").unwrap(),
        );
    }

    #[test]
    fn a_system_attribute_rejects_the_wrong_type() {
        assert_eq!(kind_of("$owner = i32(5)"), ParseErrorKind::Type);
        assert_eq!(kind_of("$expiresAt = str('soon')"), ParseErrorKind::Type);
        assert_eq!(kind_of("$contentType = i32(1)"), ParseErrorKind::Type);
        assert_eq!(kind_of("$key = true"), ParseErrorKind::Type);
    }

    #[test]
    fn unqueryable_and_renamed_system_attributes_say_so() {
        for (query, hint) in [
            ("$updatedAt > 1", "projections only"),
            ("$creationFlags = true", "projections only"),
            ("$payload = str('x')", "no index"),
            ("$expiration > 1", "$expiresAt"),
            ("$createdAtBlock > 1", "$createdAt"),
            ("$nope = true", "unknown system attribute"),
        ] {
            let err = parse(query).unwrap_err();
            assert_eq!(err.kind, ParseErrorKind::Type, "{query}");
            assert!(err.message.contains(hint), "{query}: {err}");
        }
    }

    // ── what this version deliberately leaves out ───────────────────────

    #[test]
    fn cut_operators_name_their_replacement() {
        let err = parse("level != i32(10)").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::Type);
        assert!(err.message.contains("NOT (attr = value)"), "{err}");

        let err = parse("exists(reviewedBy)").unwrap_err();
        assert!(err.message.contains("presence"), "{err}");

        let err = parse("typeof(level) = i32").unwrap_err();
        assert!(err.message.contains("not supported"), "{err}");
    }

    #[test]
    fn the_removed_symbol_operators_are_gone() {
        for query in [
            "a = true && b = true",
            "a = true || b = true",
            "!(a = true)",
            "name ~ 'ab*'",
        ] {
            assert!(parse(query).is_err(), "{query}");
        }
        // IN is gone too — write it as an OR chain.
        assert!(parse("color IN (str('a') str('b'))").is_err());
    }

    // ── boolean structure ───────────────────────────────────────────────

    #[test]
    fn precedence_is_not_then_and_then_or() {
        // AND binds tighter than OR.
        assert!(matches!(
            parse("a = true OR b = true AND c = true").unwrap(),
            Query::Or(_, right) if matches!(*right, Query::And(_, _))
        ));
        // NOT binds tighter than AND, and needs no parentheses.
        assert!(matches!(
            parse("NOT a = true AND b = true").unwrap(),
            Query::And(left, _) if matches!(*left, Query::Not(_))
        ));
        // Parentheses override.
        assert!(matches!(
            parse("(a = true OR b = true) AND c = true").unwrap(),
            Query::And(left, _) if matches!(*left, Query::Or(_, _))
        ));
    }

    #[test]
    fn connectives_are_left_associative() {
        assert!(matches!(
            parse("a = true AND b = true AND c = true").unwrap(),
            Query::And(left, _) if matches!(*left, Query::And(_, _))
        ));
        assert!(matches!(
            parse("a = true OR b = true OR c = true").unwrap(),
            Query::Or(left, _) if matches!(*left, Query::Or(_, _))
        ));
    }

    #[test]
    fn nested_nots_and_groups_collapse_cleanly() {
        assert!(matches!(
            parse("NOT NOT a = true").unwrap(),
            Query::Not(inner) if matches!(*inner, Query::Not(_))
        ));
        assert!(matches!(parse("((a = true))").unwrap(), Query::Eq { .. }));
    }

    #[test]
    fn the_star_selector_stands_alone() {
        assert_eq!(parse("*").unwrap(), Query::All);
        assert_eq!(parse("  *  ").unwrap(), Query::All);
        assert!(parse("* AND a = true").is_err());
        // `$all` was an internal name, not part of the language.
        assert!(parse("$all").is_err());
    }

    // ── lexical surface ─────────────────────────────────────────────────

    #[test]
    fn comments_and_whitespace_are_insignificant() {
        let spread = "
            level >= i32(10)   -- at least ten
        AND name  =  str('Bob') -- and named Bob
        ";
        assert_eq!(
            parse(spread).unwrap(),
            parse("level >= i32(10) AND name = str('Bob')").unwrap(),
        );
    }

    #[test]
    fn attribute_names_are_case_sensitive() {
        assert_ne!(
            parse("Level = true").unwrap(),
            parse("level = true").unwrap()
        );
    }

    #[test]
    fn reserved_and_type_names_cannot_be_attributes() {
        // Keywords lex as keywords, so they never reach an attribute position.
        assert_eq!(kind_of("and = true"), ParseErrorKind::Syntax);
        assert_eq!(kind_of("not = true"), ParseErrorKind::Syntax);
        // A type name is only a tag before `(`; bare, it is rejected by name.
        let err = parse("str = true").unwrap_err();
        assert!(err.message.contains("type name"), "{err}");
    }

    #[test]
    fn over_long_attribute_names_are_rejected() {
        let long = "a".repeat(limits::MAX_ATTRIBUTE_NAME_BYTES + 1);
        assert!(parse(&format!("{long} = true")).is_err());
        let at_limit = "a".repeat(limits::MAX_ATTRIBUTE_NAME_BYTES);
        assert!(parse(&format!("{at_limit} = true")).is_ok());
    }

    #[test]
    fn malformed_queries_are_syntax_errors_with_positions() {
        for query in [
            "",
            "level =",
            "= i32(1)",
            "level",
            "level = i32(1) AND",
            "(level = i32(1)",
            "level = = i32(1)",
            "level = i32(1) name = str('x')",
        ] {
            let err = parse(query).unwrap_err();
            assert_eq!(err.kind, ParseErrorKind::Syntax, "{query}: {err}");
            assert!(err.position.is_some(), "{query} should carry a position");
        }
    }

    // ── limits ──────────────────────────────────────────────────────────

    #[test]
    fn oversized_queries_are_limit_errors() {
        let long = format!("name = str('{}')", "a".repeat(limits::MAX_QUERY_BYTES));
        assert_eq!(parse(&long).unwrap_err().kind, ParseErrorKind::Limit);

        let many = (0..=limits::MAX_PREDICATES)
            .map(|index| format!("a{index} = true"))
            .collect::<alloc::vec::Vec<_>>()
            .join(" AND ");
        assert_eq!(parse(&many).unwrap_err().kind, ParseErrorKind::Limit);

        // Deep nesting is bounded, so neither the parser nor the evaluator can
        // be driven into unbounded recursion from an RPC call.
        let depth = limits::MAX_NESTING_DEPTH + 1;
        let deep = format!("{}a = true{}", "(".repeat(depth), ")".repeat(depth));
        assert_eq!(parse(&deep).unwrap_err().kind, ParseErrorKind::Limit);
        let nots = format!("{}a = true", "NOT ".repeat(depth));
        assert_eq!(parse(&nots).unwrap_err().kind, ParseErrorKind::Limit);
    }

    #[test]
    fn error_kinds_map_to_the_spec_codes() {
        assert_eq!(ParseErrorKind::Syntax.rpc_code(), -32001);
        assert_eq!(ParseErrorKind::Type.rpc_code(), -32002);
        assert_eq!(ParseErrorKind::Literal.rpc_code(), -32003);
        assert_eq!(ParseErrorKind::Limit.rpc_code(), -32004);
    }

    /// The spec's worked example, minus the predicates this version cuts.
    #[test]
    fn the_specs_example_query_parses() {
        let query = format!(
            "    level       >= i32(10)
             AND balance     >  u256(1000000)
             AND score       >= dec(3.5)
             AND score       <= dec(5)
             AND name        =  str('Bob')
             AND desc        STARTSWITH str('ab')
             AND parent      =  key({})
             AND hash        =  bytes32({})
             AND flagged     =  true
             AND $owner      =  addr({})
             AND $expiresAt  <  u64(1200000)",
            word_hex("12"),
            word_hex("45"),
            addr_hex("ab"),
        );
        assert!(parse(&query).is_ok(), "{:?}", parse(&query).unwrap_err());
    }
}
