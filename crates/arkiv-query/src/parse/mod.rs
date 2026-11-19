//! Recursive-descent parser: tokens → [`Query`], or a whole [`Statement`].
//!
//! ```text
//! statement  → top [ orderBy ]          -- orderBy: see `order`
//! top        → '*' | expr
//! expr       → andExpr { OR andExpr }
//! andExpr    → unary { AND unary }
//! unary      → NOT unary | primary
//! primary    → '(' expr ')' | predicate
//! predicate  → attrRef compOp value | attrRef BETWEEN value AND value
//!            | attrRef STARTSWITH strValue
//!            | EXISTS '(' userName ')' | TYPEOF '(' userName ')' '=' typeTag
//! attrRef    → userName | '$' name
//! userName   → name | '"' name '"'
//! ```
//!
//! Precedence, tightest first: `NOT`, `AND`, `OR`. The `AND` inside a
//! `BETWEEN` is the predicate's own, not a conjunction.
//!
//! **No name is reserved.** A bare keyword is always the keyword, so an
//! attribute named like one is written quoted, `"and" = true`; any other name
//! may be quoted or not, to the same effect. Type tags and the words of
//! `ORDER BY` are read by position, so `str = str('x')` and `desc = 1` need no
//! quotes at all.
//!
//! Two jobs beyond shape. First, **typing**: a value carries its own type, and
//! this is where a value is checked against the attribute it is compared to
//! (`$owner` takes an address) and against its operator. Every type the
//! language can write is ordered, so the comparisons take any of them, but
//! only a `str` has a prefix: `STARTSWITH` on anything else is an error here,
//! not an empty result later — the spec is explicit that the two must not be
//! confused. Second, **limits**: length, predicate count, nesting depth and
//! `AND`/`OR` depth are bounded, because `parse` runs on unauthenticated RPC
//! input.
//!
//! A chain of one connective is one node: `a AND b AND c` is an `And` of
//! three, so the tree is only as deep as the query nests.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

mod order;

use arkiv_interfaces::entity::{AttributeType, AttributeValue};
use arkiv_interfaces::query::{AnnotKey, AnnotVal, BuiltIn, Query, Statement, ValueRange};

use crate::error::{ParseError, ParseErrorKind};
use crate::lexer::{SpannedToken, Token, TypeTag, is_name_char, tokenize};
use crate::limits;
use crate::literal;

/// Parse a query's predicates into a [`Query`] AST, refusing an `ORDER BY`.
/// See [`parse_statement`] for a whole query string, and the
/// [module docs](self).
pub fn parse(input: &str) -> Result<Query, ParseError> {
    let mut parser = Parser::new(input)?;
    let filter = parser.parse_filter()?;
    if let Some(position) = parser.word_at("order") {
        return Err(ParseError::syntax(
            position,
            "ORDER BY is not taken here — this reads a query's predicates only",
        ));
    }
    parser.expect_end()?;
    Ok(filter)
}

/// Parse a whole query string, its predicates and the `ORDER BY` it may end
/// with, into a [`Statement`].
pub fn parse_statement(input: &str) -> Result<Statement, ParseError> {
    let mut parser = Parser::new(input)?;
    let filter = parser.parse_filter()?;
    let order = match parser.word_at("order") {
        Some(_) => Some(parser.parse_order_by()?),
        None => {
            parser.expect_end()?;
            None
        }
    };
    Ok(Statement { filter, order })
}

struct Parser<'a> {
    /// The query text, for errors that quote what was written.
    src: &'a str,
    tokens: Vec<SpannedToken>,
    pos: usize,
    /// Byte offset one past the input, for "unexpected end of query" errors.
    end: usize,
    depth: usize,
    predicates: usize,
}

impl<'a> Parser<'a> {
    /// A parser over `input`'s tokens, once it is within the length cap.
    fn new(input: &'a str) -> Result<Self, ParseError> {
        if input.len() > limits::MAX_QUERY_BYTES {
            return Err(ParseError::whole(
                ParseErrorKind::QueryLimitError,
                "query is too long",
            ));
        }
        Ok(Self {
            src: input,
            tokens: tokenize(input)?,
            pos: 0,
            end: input.len(),
            depth: 0,
            predicates: 0,
        })
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

    /// The predicates: `*`, or an expression. What may follow them, an
    /// `ORDER BY` or nothing, is the caller's to check.
    fn parse_filter(&mut self) -> Result<Query, ParseError> {
        if self.tokens.is_empty() {
            return Err(ParseError::syntax(
                0,
                "empty query — write a predicate, or * to match every entity",
            ));
        }
        if matches!(self.peek_token(), Some(Token::Star)) {
            self.advance();
            let stands_alone = self.peek().is_none() || self.word_at("order").is_some();
            if !stands_alone {
                return Err(ParseError::syntax(
                    self.next_position(),
                    "* matches every entity and cannot be combined with other predicates",
                ));
            }
            return Ok(Query::All);
        }
        self.parse_expr()
    }

    /// Nothing left to read.
    fn expect_end(&self) -> Result<(), ParseError> {
        match self.peek() {
            Some(spanned) => Err(ParseError::syntax(
                spanned.start,
                "unexpected trailing input — did you mean to join these with AND or OR?",
            )),
            None => Ok(()),
        }
    }

    /// Where the next token starts, when it is the bare word `word`, in any
    /// case. The words of `ORDER BY` are read this way, where the clause
    /// stands, so none of them is reserved. A quoted word is a name, so
    /// `"order"` never opens the clause.
    fn word_at(&self, word: &str) -> Option<usize> {
        match self.peek() {
            Some(SpannedToken {
                token: Token::Name(name),
                start,
            }) if name.eq_ignore_ascii_case(word) => Some(*start),
            _ => None,
        }
    }

    fn parse_expr(&mut self) -> Result<Query, ParseError> {
        let first = self.parse_and()?;
        self.chain(first, &Token::Or, Self::parse_and, Query::Or)
    }

    fn parse_and(&mut self) -> Result<Query, ParseError> {
        let first = self.parse_unary()?;
        self.chain(first, &Token::And, Self::parse_unary, Query::And)
    }

    /// `first` and every operand joined to it by `connective`, as one node of
    /// them all: `a AND b AND c` is one `And` of three, not nested pairs. A
    /// lone operand is returned as it is.
    ///
    /// Each operand is held to [`limits::MAX_AND_OR_DEPTH`] as soon as it is
    /// parsed, so a query nesting deeper stops here, before anything deeper
    /// than the limit is built.
    fn chain(
        &mut self,
        first: Query,
        connective: &Token,
        operand: fn(&mut Self) -> Result<Query, ParseError>,
        node: fn(Vec<Query>) -> Query,
    ) -> Result<Query, ParseError> {
        let Some(mut position) = self.connective_at(connective) else {
            return Ok(first);
        };
        // A node is coming, so `first` is one of its operands.
        within_and_or_depth(&first, position)?;
        let mut operands = vec![first];
        loop {
            self.advance();
            let next = operand(self)?;
            within_and_or_depth(&next, position)?;
            operands.push(next);
            let Some(next_position) = self.connective_at(connective) else {
                return Ok(node(operands));
            };
            position = next_position;
        }
    }

    /// Where the next token starts, when it is `connective`.
    fn connective_at(&self, connective: &Token) -> Option<usize> {
        self.peek()
            .filter(|spanned| spanned.token == *connective)
            .map(|spanned| spanned.start)
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
        if self.operator_follows_keyword() {
            return Err(self.keyword_as_name(start));
        }
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
                ParseErrorKind::QueryLimitError,
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
                ParseErrorKind::QueryLimitError,
                "query has too many predicates",
            ));
        }

        // `exists(name)` and `typeof(name) = tag` lead with their keyword
        // rather than an attribute reference, so they are dispatched first.
        match self.peek_token() {
            Some(Token::Exists) => return self.parse_presence(Presence::Exists),
            Some(Token::TypeOf) => return self.parse_presence(Presence::TypeOf),
            _ => {}
        }

        let key = self.parse_attr_ref()?;
        let Some(operator) = self.advance() else {
            return Err(ParseError::syntax(
                self.end,
                "expected a comparison operator (= < <= > >= BETWEEN STARTSWITH) after the \
                 attribute",
            ));
        };

        match operator.token {
            Token::Eq | Token::Lt | Token::Lte | Token::Gt | Token::Gte => {
                let (value, _) = self.parse_value(&key)?;
                Ok(comparison(&operator.token, key, value))
            }
            Token::Between => self.parse_between(key),
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
                "expected a comparison operator (= < <= > >= BETWEEN STARTSWITH)",
            )),
        }
    }

    /// `rangeTest = attrRef , BETWEEN , value , AND , value`, after the
    /// `BETWEEN`. The `AND` is taken here, as the predicate's own, so
    /// `a BETWEEN 1 AND 5 AND b = 2` is `(a BETWEEN 1 AND 5) AND b = 2`.
    fn parse_between(&mut self, key: AnnotKey) -> Result<Query, ParseError> {
        let (low, _) = self.parse_value(&key)?;
        self.expect(
            &Token::And,
            self.next_position(),
            "BETWEEN joins its two bounds with AND — a BETWEEN lo AND hi",
        )?;
        let (high, position) = self.parse_value(&key)?;
        let types = (low.attr_type().name(), high.attr_type().name());
        let bounds = ValueRange::new(low, high).ok_or_else(|| {
            ParseError::type_error(
                position,
                alloc::format!(
                    "BETWEEN bounds must be of one type, but they are {} and {}",
                    types.0,
                    types.1,
                ),
            )
        })?;
        Ok(Query::Between { key, bounds })
    }

    /// `existsTest = EXISTS , "(" , userName , ")"` and
    /// `typeTest = TYPEOF , "(" , userName , ")" , "=" , typeTag`, per the spec's
    /// grammar (`docs/api-spec.md` §4).
    ///
    /// Both take `userName`, not `attrRef`: a system attribute is carried by
    /// every entity, so asking whether it is there would be a constant.
    fn parse_presence(&mut self, which: Presence) -> Result<Query, ParseError> {
        let keyword = self.advance().expect("the caller peeked EXISTS or TYPEOF");
        if self.operator_follows_keyword() {
            return Err(self.keyword_as_name(keyword.start));
        }
        self.expect(
            &Token::LParen,
            keyword.start,
            match which {
                Presence::Exists => "exists takes a parenthesized attribute name — exists(name)",
                Presence::TypeOf => {
                    "typeof takes a parenthesized attribute name — typeof(name) = tag"
                }
            },
        )?;
        let name = self.parse_presence_name()?;
        self.expect(
            &Token::RParen,
            self.next_position(),
            "expected ) after the attribute name",
        )?;

        let attr_type = match which {
            Presence::Exists => None,
            Presence::TypeOf => {
                self.expect(
                    &Token::Eq,
                    self.next_position(),
                    "typeof names the type it asks for — typeof(name) = tag",
                )?;
                Some(self.parse_type_tag(TagSite::TypeOf)?)
            }
        };
        Ok(Query::Exists { name, attr_type })
    }

    /// The `name` inside `exists(…)` / `typeof(…)`: a user attribute only.
    fn parse_presence_name(&mut self) -> Result<String, ParseError> {
        let Some(spanned) = self.advance() else {
            return Err(ParseError::syntax(self.end, "expected an attribute name"));
        };
        match spanned.token {
            Token::Name(name) | Token::QuotedName(name) => {
                validate_user_name(&name, spanned.start)?;
                Ok(name)
            }
            Token::SysName(name) => Err(ParseError::type_error(
                spanned.start,
                alloc::format!(
                    "${name} is a system attribute, which every entity carries — \
                     exists and typeof take a user attribute"
                ),
            )),
            token if token.is_keyword() => Err(self.keyword_as_name(spanned.start)),
            _ => Err(ParseError::syntax(
                spanned.start,
                "expected an attribute name",
            )),
        }
    }

    /// A bare type tag, not a literal: `typeof`'s right-hand side, or the type
    /// after an `ORDER BY` attribute's `:`.
    ///
    /// Tags are positional rather than reserved, so one arrives as a plain
    /// name; `bytes` is not among them, which is what keeps the one type that
    /// is never indexed out of both.
    fn parse_type_tag(&mut self, site: TagSite) -> Result<AttributeType, ParseError> {
        let Some(spanned) = self.advance() else {
            return Err(ParseError::syntax(
                self.end,
                alloc::format!("expected a type tag, e.g. {}", site.form("i32")),
            ));
        };
        match spanned.token {
            Token::Name(name) => match TypeTag::from_name(&name) {
                Some(tag) => Ok(attr_type_of(tag)),
                None => Err(ParseError::type_error(
                    spanned.start,
                    alloc::format!(
                        "{name} is not a type tag — one of \
                         i32 u64 u256 dec str addr key bytes32 bool"
                    ),
                )),
            },
            Token::Tagged { tag, .. } => Err(ParseError::type_error(
                spanned.start,
                alloc::format!(
                    "{} asks for a bare type tag, not a value — write {}",
                    site.operator(),
                    site.form(tag.name())
                ),
            )),
            Token::QuotedName(name) => Err(ParseError::syntax(
                spanned.start,
                alloc::format!(
                    "\"{name}\" is quoted, so it is an attribute name — a type tag is \
                     written bare: {}",
                    site.form(TypeTag::from_name(&name).map_or("i32", TypeTag::name))
                ),
            )),
            _ => Err(ParseError::syntax(
                spanned.start,
                alloc::format!("expected a type tag, e.g. {}", site.form("i32")),
            )),
        }
    }

    /// Whether the keyword just read, `NOT`, `EXISTS` or `TYPEOF`, was meant
    /// as an attribute name, because an operator follows it: `not = true`.
    /// Not when that operator is `BETWEEN` or `STARTSWITH` and is itself
    /// followed by an operator, as in `NOT between = true`: then it is the
    /// operator's word that was meant as the name.
    fn operator_follows_keyword(&self) -> bool {
        let token = |offset: usize| self.tokens.get(self.pos + offset).map(|t| &t.token);
        match token(0) {
            Some(next) if next.is_operator() => {
                !(next.is_keyword() && token(1).is_some_and(Token::is_operator))
            }
            Some(_) | None => false,
        }
    }

    /// A keyword standing where an attribute name belongs. Bare, it can only
    /// be the keyword, so the error shows the quoted form, which keeps the
    /// spelling it was written in: names are case-sensitive.
    fn keyword_as_name(&self, start: usize) -> ParseError {
        let rest = &self.src[start..];
        let word = &rest[..rest.find(|ch| !is_name_char(ch)).unwrap_or(rest.len())];
        ParseError::syntax(
            start,
            alloc::format!(
                "{word} is a keyword — to name an attribute {word}, quote it: \"{word}\""
            ),
        )
    }

    /// Consume `expected`, or report `message` against `position`.
    fn expect(
        &mut self,
        expected: &Token,
        position: usize,
        message: &str,
    ) -> Result<(), ParseError> {
        if self.peek_token() == Some(expected) {
            self.advance();
            return Ok(());
        }
        Err(ParseError::syntax(position, message))
    }

    fn parse_attr_ref(&mut self) -> Result<AnnotKey, ParseError> {
        let Some(spanned) = self.advance() else {
            return Err(ParseError::syntax(self.end, "expected an attribute name"));
        };
        match spanned.token {
            Token::Name(name) | Token::QuotedName(name) => {
                validate_user_name(&name, spanned.start)?;
                Ok(AnnotKey::User(name))
            }
            Token::SysName(name) => builtin_from_name(&name, spanned.start).map(AnnotKey::BuiltIn),
            token if token.is_keyword() => Err(self.keyword_as_name(spanned.start)),
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
            // `name = "Bob"` is an easy slip: here double quotes make a name.
            Token::QuotedName(name) => {
                return Err(ParseError::syntax(
                    position,
                    alloc::format!(
                        "\"{name}\" is an attribute name — a string value is single-quoted \
                         and tagged: str('{name}')"
                    ),
                ));
            }
            _ => {
                return Err(ParseError::syntax(
                    position,
                    "expected a value, e.g. i32(10) or str('Bob')",
                ));
            }
        };
        check_value_type(key, &value, position)?;
        Ok((value, position))
    }
}

/// Where a bare type tag stands, which its errors show the form of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagSite {
    /// `typeof(name) = tag`.
    TypeOf,
    /// `ORDER BY name:tag`.
    OrderBy,
}

impl TagSite {
    /// The construct the tag belongs to, as an error names it.
    fn operator(self) -> &'static str {
        match self {
            Self::TypeOf => "typeof",
            Self::OrderBy => "ORDER BY",
        }
    }

    /// The construct written out with `tag`.
    fn form(self, tag: &str) -> String {
        match self {
            Self::TypeOf => alloc::format!("typeof(name) = {tag}"),
            Self::OrderBy => alloc::format!("ORDER BY name:{tag}"),
        }
    }
}

/// Which presence operator a predicate opened with. The two share a parse —
/// a parenthesized name — and differ only in whether a type follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// `exists(name)`: the attribute, under any type.
    Exists,
    /// `typeof(name) = tag`: the attribute, under exactly one type.
    TypeOf,
}

/// The attribute type a type tag names.
///
/// Total: every tag in the language denotes an entity attribute type. `bytes`
/// has no tag, which is how the one never-indexed type stays out of `typeof`.
const fn attr_type_of(tag: TypeTag) -> AttributeType {
    match tag {
        TypeTag::Bool => AttributeType::Bool,
        TypeTag::I32 => AttributeType::Int,
        TypeTag::U64 => AttributeType::U64,
        TypeTag::U256 => AttributeType::U256,
        TypeTag::Dec => AttributeType::Decimal,
        TypeTag::Bytes32 => AttributeType::Bytes32,
        TypeTag::Str => AttributeType::Str,
        TypeTag::Addr => AttributeType::EthereumAddress,
        TypeTag::Key => AttributeType::EntityKey,
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
/// the value is keyed in the store.
fn builtin_type(field: BuiltIn) -> AttributeType {
    match field {
        BuiltIn::Owner | BuiltIn::Creator => AttributeType::EthereumAddress,
        BuiltIn::Key => AttributeType::EntityKey,
        BuiltIn::ExpiresAt => AttributeType::U64,
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

/// Refuse `operand` when the `AND`/`OR` node at `position` joining it would
/// nest more than [`limits::MAX_AND_OR_DEPTH`] levels: the node is one level
/// above its deepest operand.
fn within_and_or_depth(operand: &Query, position: usize) -> Result<(), ParseError> {
    if and_or_depth(operand) < limits::MAX_AND_OR_DEPTH {
        return Ok(());
    }
    Err(ParseError::at(
        position,
        ParseErrorKind::QueryLimitError,
        alloc::format!(
            "query nests AND/OR more than {} levels deep",
            limits::MAX_AND_OR_DEPTH
        ),
    ))
}

/// How many `AND`/`OR` levels `query` nests: none for a predicate, and one
/// more than its deepest operand for an `AND` or `OR`. A `NOT` adds none; its
/// nesting is held to [`limits::MAX_NESTING_DEPTH`] instead.
fn and_or_depth(query: &Query) -> usize {
    match query {
        Query::And(operands) | Query::Or(operands) => {
            1 + operands.iter().map(and_or_depth).max().unwrap_or(0)
        }
        Query::Not(inner) => and_or_depth(inner),
        Query::All
        | Query::Eq { .. }
        | Query::Gt { .. }
        | Query::Gte { .. }
        | Query::Lt { .. }
        | Query::Lte { .. }
        | Query::Between { .. }
        | Query::StartsWith { .. }
        | Query::Exists { .. } => 0,
    }
}

/// User attribute names, bare or quoted: `[A-Za-z][A-Za-z0-9_.\-]*`, at most
/// 32 bytes.
///
/// The charset is the lexer's business, so what is left is the length cap.
/// No name is reserved: a type tag is a tag only where one stands, so a bare
/// `str` here is an attribute named `str`, and a keyword reaches here only
/// quoted, as the name it then is.
fn validate_user_name(name: &str, position: usize) -> Result<(), ParseError> {
    if name.len() > limits::MAX_ATTRIBUTE_NAME_BYTES {
        return Err(ParseError::syntax(
            position,
            "attribute names are limited to 32 bytes",
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

        "createdAt" => Err(ParseError::type_error(
            position,
            "$createdAt is not queryable — it is returned by projections only",
        )),
        "updatedAt" => Err(ParseError::type_error(
            position,
            "$updatedAt is not queryable — it is returned by projections only",
        )),
        "creationFlags" => Err(ParseError::type_error(
            position,
            "$creationFlags is not queryable — it is returned by projections only",
        )),
        "payload" | "contentType" => Err(ParseError::type_error(
            position,
            alloc::format!(
                "${name} is no longer a system attribute — content type and payload are \
                 ordinary user attributes now; query yours by its own name, unprefixed"
            ),
        )),

        // The pre-spec spellings, named so an old query says what to change.
        "expiration" => Err(ParseError::type_error(
            position,
            "unknown system attribute $expiration — it is now $expiresAt",
        )),
        "createdAtBlock" => Err(ParseError::type_error(
            position,
            "unknown system attribute $createdAtBlock — it is now $createdAt, which is \
             returned by projections only",
        )),

        _ => Err(ParseError::type_error(
            position,
            alloc::format!(
                "unknown system attribute ${name} — expected $key, $owner, $creator or \
                 $expiresAt"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeSet;
    use alloc::format;
    use alloc::string::ToString;
    use arkiv_interfaces::query::{Direction, OrderBy, words};

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
        assert_eq!(kind_of("level = 2147483648"), ParseErrorKind::LiteralError);
        // Bare strings still have no default.
        assert_eq!(kind_of("name = 'Bob'"), ParseErrorKind::TypeError);
    }

    #[test]
    fn literal_validation_errors_are_their_own_kind() {
        assert_eq!(
            kind_of("level = i32(2147483648)"),
            ParseErrorKind::LiteralError
        );
        assert_eq!(
            kind_of("score = dec(0.1234567890123456789)"),
            ParseErrorKind::LiteralError
        );
        assert_eq!(kind_of("who = addr(0xdead)"), ParseErrorKind::LiteralError);
        assert_eq!(
            kind_of("parent = key(0xdead)"),
            ParseErrorKind::LiteralError
        );
    }

    // ── the operator × type matrix ──────────────────────────────────────

    /// Every type the language can write is ordered, so each takes every
    /// range operator.
    #[test]
    fn ranges_are_allowed_on_every_type() {
        let values = [
            "i32(1)".to_string(),
            "u64(1)".to_string(),
            "u256(1)".to_string(),
            "dec(3.5)".to_string(),
            "true".to_string(),
            "str('a')".to_string(),
            format!("addr({})", addr_hex("ab")),
            format!("key({})", word_hex("ab")),
            format!("bytes32({})", word_hex("ab")),
        ];
        for value in &values {
            for operator in ["<", "<=", ">", ">="] {
                let query = format!("a {operator} {value}");
                assert!(parse(&query).is_ok(), "{query}: {:?}", parse(&query));
            }
        }
        assert_eq!(
            parse("name > str('a')").unwrap(),
            Query::Gt {
                key: user("name"),
                value: AttributeValue::Str("a".into()),
            }
        );
        assert_eq!(
            parse("flagged <= false").unwrap(),
            Query::Lte {
                key: user("flagged"),
                value: AttributeValue::Bool(false),
            }
        );
    }

    /// The spec gives `$key`, `$owner` and `$creator` every comparison.
    #[test]
    fn ranges_are_allowed_on_the_system_attributes() {
        assert_eq!(
            parse(&format!("$owner > addr({})", addr_hex("ab"))).unwrap(),
            Query::Gt {
                key: built_in(BuiltIn::Owner),
                value: AttributeValue::EthereumAddress([0xab; 20]),
            }
        );
        assert!(parse(&format!("$creator <= addr({})", addr_hex("ab"))).is_ok());
        assert_eq!(
            parse(&format!("$key >= key({})", word_hex("ab"))).unwrap(),
            Query::Gte {
                key: built_in(BuiltIn::Key),
                value: AttributeValue::EntityKey([0xab; 32]),
            }
        );
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
        assert_eq!(
            kind_of("level STARTSWITH i32(1)"),
            ParseErrorKind::TypeError
        );
        assert_eq!(
            kind_of("flagged STARTSWITH true"),
            ParseErrorKind::TypeError
        );
    }

    // ── BETWEEN ─────────────────────────────────────────────────────────

    /// `name BETWEEN i32(low) AND i32(high)`.
    fn between_ints(name: &str, low: i32, high: i32) -> Query {
        Query::Between {
            key: user(name),
            bounds: ValueRange::new(AttributeValue::Int(low), AttributeValue::Int(high))
                .expect("one type"),
        }
    }

    #[test]
    fn between_takes_two_bounds_of_one_type() {
        assert_eq!(
            parse("level BETWEEN 1 AND 5").unwrap(),
            between_ints("level", 1, 5)
        );
        assert_eq!(
            parse("level between i32(-5) and i32(5)").unwrap(),
            between_ints("level", -5, 5)
        );
        // Every type with a literal ranges, so every one has a BETWEEN.
        for (low, high) in [
            ("u64(1)".to_string(), "u64(9)".to_string()),
            ("u256(1)".to_string(), "u256(0x10)".to_string()),
            ("dec(3.5)".to_string(), "dec(5)".to_string()),
            ("false".to_string(), "true".to_string()),
            ("str('a')".to_string(), "str('b')".to_string()),
            (
                format!("addr({})", addr_hex("01")),
                format!("addr({})", addr_hex("ab")),
            ),
            (
                format!("key({})", word_hex("01")),
                format!("key({})", word_hex("ab")),
            ),
            (
                format!("bytes32({})", word_hex("01")),
                format!("bytes32({})", word_hex("ab")),
            ),
        ] {
            let query = format!("a BETWEEN {low} AND {high}");
            assert!(parse(&query).is_ok(), "{query}: {:?}", parse(&query));
        }
        // A low bound above the high one is a query that matches nothing, not
        // an error.
        assert_eq!(
            parse("level BETWEEN 5 AND 1").unwrap(),
            between_ints("level", 5, 1)
        );
    }

    /// The `AND` inside a `BETWEEN` is its own, wherever the predicate stands.
    #[test]
    fn between_owns_its_and() {
        assert_eq!(
            parse("a BETWEEN 1 AND 5 AND b = true").unwrap(),
            Query::And(vec![between_ints("a", 1, 5), is_true("b")])
        );
        assert_eq!(
            parse("b = true AND a BETWEEN 1 AND 5 AND c = true").unwrap(),
            Query::And(vec![is_true("b"), between_ints("a", 1, 5), is_true("c")])
        );
        assert_eq!(
            parse("b = true OR a BETWEEN 1 AND 5").unwrap(),
            Query::Or(vec![is_true("b"), between_ints("a", 1, 5)])
        );
        // NOT takes the whole predicate, bounds and all.
        assert_eq!(
            parse("NOT a BETWEEN 1 AND 5").unwrap(),
            Query::Not(Box::new(between_ints("a", 1, 5)))
        );
    }

    #[test]
    fn between_bounds_of_two_types_are_a_type_error() {
        let err = parse("a BETWEEN i32(1) AND u64(5)").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::TypeError);
        assert!(err.message.contains("i32 and u64"), "{err}");
        assert_eq!(err.failure_position, Some("a BETWEEN i32(1) AND ".len()));
    }

    #[test]
    fn between_on_a_system_attribute_takes_its_type() {
        assert_eq!(
            parse("$expiresAt BETWEEN u64(10) AND u64(20)").unwrap(),
            Query::Between {
                key: built_in(BuiltIn::ExpiresAt),
                bounds: ValueRange::new(AttributeValue::U64(10), AttributeValue::U64(20)).unwrap(),
            }
        );
        // `$owner`, `$creator` and `$key` take every comparison, BETWEEN too.
        assert_eq!(
            parse(&format!(
                "$owner BETWEEN addr({}) AND addr({})",
                addr_hex("01"),
                addr_hex("ab")
            ))
            .unwrap(),
            Query::Between {
                key: built_in(BuiltIn::Owner),
                bounds: ValueRange::new(
                    AttributeValue::EthereumAddress([0x01; 20]),
                    AttributeValue::EthereumAddress([0xab; 20])
                )
                .unwrap(),
            }
        );
        assert!(
            parse(&format!(
                "$creator BETWEEN addr({}) AND addr({})",
                addr_hex("01"),
                addr_hex("ab")
            ))
            .is_ok()
        );
        assert_eq!(
            parse(&format!(
                "$key BETWEEN key({}) AND key({})",
                word_hex("01"),
                word_hex("ab")
            ))
            .unwrap(),
            Query::Between {
                key: built_in(BuiltIn::Key),
                bounds: ValueRange::new(
                    AttributeValue::EntityKey([0x01; 32]),
                    AttributeValue::EntityKey([0xab; 32])
                )
                .unwrap(),
            }
        );
        // Each bound is held to the attribute's type, as any value is.
        assert_eq!(
            kind_of("$expiresAt BETWEEN 10 AND 20"),
            ParseErrorKind::TypeError
        );
        assert_eq!(
            kind_of(&format!(
                "$owner BETWEEN addr({}) AND u64(1)",
                addr_hex("ab")
            )),
            ParseErrorKind::TypeError
        );
    }

    #[test]
    fn between_needs_its_full_shape() {
        for query in [
            "a BETWEEN",
            "a BETWEEN 1",
            "a BETWEEN 1 5",
            "a BETWEEN 1 OR 5",
            "a BETWEEN AND 5",
            "a BETWEEN 1 AND",
        ] {
            assert_eq!(
                kind_of(query),
                ParseErrorKind::MalformedInputError,
                "`{query}` should not parse"
            );
        }
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
                // A block height is a u64 on the surface and a U64 cell in
                // the store, so the literal travels unchanged.
                value: AttributeValue::U64(1_200_000),
            }
        );

        // Untagged is a type error that names the fix, rather than silently
        // comparing an i32 against a u64 attribute and matching nothing.
        let err = parse("$expiresAt < 1200000").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::TypeError);
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
    }

    #[test]
    fn a_system_attribute_rejects_the_wrong_type() {
        assert_eq!(kind_of("$owner = i32(5)"), ParseErrorKind::TypeError);
        assert_eq!(
            kind_of("$expiresAt = str('soon')"),
            ParseErrorKind::TypeError
        );
        assert_eq!(kind_of("$key = true"), ParseErrorKind::TypeError);
    }

    #[test]
    fn unqueryable_and_renamed_system_attributes_say_so() {
        for (query, hint) in [
            ("$createdAt > u64(1)", "projections only"),
            ("$updatedAt > 1", "projections only"),
            ("$creationFlags = true", "projections only"),
            ("$payload = str('x')", "user attributes now"),
            ("$contentType = str('x')", "user attributes now"),
            ("$expiration > 1", "$expiresAt"),
            ("$createdAtBlock > 1", "$createdAt"),
            ("$nope = true", "unknown system attribute"),
        ] {
            let err = parse(query).unwrap_err();
            assert_eq!(err.kind, ParseErrorKind::TypeError, "{query}");
            assert!(err.message.contains(hint), "{query}: {err}");
        }
    }

    // ── the presence operators ──────────────────────────────────────────

    /// `existsTest = EXISTS , "(" , name , ")"`.
    #[test]
    fn exists_takes_a_parenthesized_user_attribute() {
        assert_eq!(
            parse("exists(reviewedBy)").unwrap(),
            Query::Exists {
                name: "reviewedBy".into(),
                attr_type: None,
            }
        );
        // Keywords are case-insensitive, like every other.
        assert_eq!(
            parse("EXISTS(reviewedBy)").unwrap(),
            parse("exists(reviewedBy)").unwrap()
        );
    }

    /// `typeTest = TYPEOF , "(" , name , ")" , "=" , typeTag`, and every tag
    /// in the language denotes a type it can ask for.
    #[test]
    fn typeof_names_the_type_it_asks_for() {
        assert_eq!(
            parse("typeof(level) = i32").unwrap(),
            Query::Exists {
                name: "level".into(),
                attr_type: Some(AttributeType::Int),
            }
        );
        for (tag, attr_type) in [
            ("bool", AttributeType::Bool),
            ("i32", AttributeType::Int),
            ("u64", AttributeType::U64),
            ("u256", AttributeType::U256),
            ("dec", AttributeType::Decimal),
            ("bytes32", AttributeType::Bytes32),
            ("str", AttributeType::Str),
            ("addr", AttributeType::EthereumAddress),
            ("key", AttributeType::EntityKey),
        ] {
            assert_eq!(
                parse(&format!("typeof(a) = {tag}")).unwrap(),
                Query::Exists {
                    name: "a".into(),
                    attr_type: Some(attr_type),
                },
                "{tag}"
            );
            // Tags are read by position, so inside the parentheses a tag is
            // just the attribute's name.
            assert_eq!(
                parse(&format!("typeof({tag}) = {tag}")).unwrap(),
                Query::Exists {
                    name: tag.into(),
                    attr_type: Some(attr_type),
                },
                "{tag}"
            );
        }
        assert_eq!(
            parse("TYPEOF(level) = I32").unwrap(),
            parse("typeof(level) = i32").unwrap()
        );
    }

    /// Both combine like any other predicate.
    #[test]
    fn presence_operators_combine_and_negate() {
        assert_eq!(
            parse("exists(a) AND level > i32(3)").unwrap(),
            Query::And(vec![
                Query::Exists {
                    name: "a".into(),
                    attr_type: None,
                },
                Query::Gt {
                    key: user("level"),
                    value: AttributeValue::Int(3),
                },
            ])
        );
        assert_eq!(
            parse("NOT exists(a)").unwrap(),
            Query::Not(Box::new(Query::Exists {
                name: "a".into(),
                attr_type: None,
            }))
        );
        assert!(parse("exists(a) OR typeof(b) = str").is_ok());
    }

    /// The grammar gives both operators `name`, not `attrRef`: every entity
    /// carries the system attributes, so asking would be a constant.
    #[test]
    fn presence_operators_refuse_a_system_attribute() {
        for query in ["exists($owner)", "typeof($owner) = addr"] {
            let err = parse(query).unwrap_err();
            assert_eq!(err.kind, ParseErrorKind::TypeError, "{query}");
            assert!(err.message.contains("system attribute"), "{query}: {err}");
        }
    }

    /// `bytes` is the one type that is never indexed, and the only one with no
    /// tag — so `typeof` cannot name it.
    #[test]
    fn typeof_refuses_a_non_tag_and_a_value() {
        let err = parse("typeof(a) = bytes").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::TypeError);
        assert!(err.message.contains("not a type tag"), "{err}");

        let err = parse("typeof(a) = str('x')").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::TypeError);
        assert!(err.message.contains("bare type tag"), "{err}");
    }

    #[test]
    fn presence_operators_need_their_full_shape() {
        for query in [
            "exists",
            "exists a",
            "exists(a",
            "exists()",
            "typeof(a)",
            "typeof(a) i32",
            "typeof(a) = ",
        ] {
            assert!(parse(query).is_err(), "`{query}` should not parse");
        }
    }

    // ── what this version deliberately leaves out ───────────────────────

    #[test]
    fn cut_operators_name_their_replacement() {
        let err = parse("level != i32(10)").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::TypeError);
        assert!(err.message.contains("NOT (attr = value)"), "{err}");
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

    /// `name = true`, the predicate the structure tests are built from.
    fn is_true(name: &str) -> Query {
        Query::Eq {
            key: user(name),
            value: AttributeValue::Bool(true),
        }
    }

    #[test]
    fn precedence_is_not_then_and_then_or() {
        // AND binds tighter than OR.
        assert_eq!(
            parse("a = true OR b = true AND c = true").unwrap(),
            Query::Or(vec![
                is_true("a"),
                Query::And(vec![is_true("b"), is_true("c")])
            ])
        );
        // NOT binds tighter than AND, and needs no parentheses.
        assert_eq!(
            parse("NOT a = true AND b = true").unwrap(),
            Query::And(vec![Query::Not(Box::new(is_true("a"))), is_true("b")])
        );
        // Parentheses override.
        assert_eq!(
            parse("(a = true OR b = true) AND c = true").unwrap(),
            Query::And(vec![
                Query::Or(vec![is_true("a"), is_true("b")]),
                is_true("c")
            ])
        );
    }

    /// A chain of one connective is one node, its operands in the written
    /// order.
    #[test]
    fn a_chain_of_one_connective_is_one_node() {
        assert_eq!(
            parse("a = true AND b = true AND c = true").unwrap(),
            Query::And(vec![is_true("a"), is_true("b"), is_true("c")])
        );
        assert_eq!(
            parse("a = true OR b = true OR c = true").unwrap(),
            Query::Or(vec![is_true("a"), is_true("b"), is_true("c")])
        );
        // A parenthesized chain is a group of its own, not merged into the
        // chain around it.
        assert_eq!(
            parse("a = true AND (b = true AND c = true)").unwrap(),
            Query::And(vec![
                is_true("a"),
                Query::And(vec![is_true("b"), is_true("c")])
            ])
        );
    }

    /// `levels` AND/OR levels, alternating and each nesting the next:
    /// `a = true OR (a = true AND (a = true))` is two.
    fn nested_and_or(levels: usize) -> String {
        (0..levels).fold(String::from("a = true"), |inner, level| {
            let connective = if level % 2 == 0 { "AND" } else { "OR" };
            format!("a = true {connective} ({inner})")
        })
    }

    /// AND/OR nest at most `MAX_AND_OR_DEPTH` levels. One more is a limit
    /// error at the connective that would add it, raised before the parser
    /// builds anything deeper.
    #[test]
    fn and_or_nest_at_most_the_depth_limit() {
        let deepest = nested_and_or(limits::MAX_AND_OR_DEPTH);
        assert!(parse(&deepest).is_ok(), "{deepest}");

        let too_deep = nested_and_or(limits::MAX_AND_OR_DEPTH + 1);
        let err = parse(&too_deep).unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::QueryLimitError, "{too_deep}");
        assert!(err.message.contains("AND/OR"), "{err}");
        assert_eq!(
            err.failure_position,
            Some("a = true ".len()),
            "at the outermost connective, which would add the level"
        );

        // A NOT between levels adds none, and hides none.
        let negated = format!("a = true AND NOT ({deepest})");
        assert_eq!(
            parse(&negated).unwrap_err().kind,
            ParseErrorKind::QueryLimitError,
            "{negated}"
        );
    }

    /// However long, a chain of one connective is a single level.
    #[test]
    fn a_long_chain_is_one_level() {
        let chain = (0..limits::MAX_PREDICATES)
            .map(|i| format!("a{i} = true"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let Query::And(operands) = parse(&chain).unwrap() else {
            panic!("one And");
        };
        assert_eq!(operands.len(), limits::MAX_PREDICATES);
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

    /// A bare keyword is the keyword, so an attribute named like one is
    /// quoted; a type name is a tag only before `(`, so it needs no quotes.
    /// The quoted names are covered in full below.
    #[test]
    fn a_bare_keyword_is_not_a_name_but_a_bare_type_name_is() {
        assert_eq!(kind_of("and = true"), ParseErrorKind::MalformedInputError);
        assert_eq!(kind_of("not = true"), ParseErrorKind::MalformedInputError);
        assert_eq!(
            kind_of("between = true"),
            ParseErrorKind::MalformedInputError
        );
        assert_eq!(parse("\"between\" = true").unwrap(), is_true("between"));
        assert_eq!(parse("str = true").unwrap(), is_true("str"));
        // Any other word is a name, `set` included.
        assert_eq!(parse("set = true").unwrap(), is_true("set"));
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
            assert_eq!(
                err.kind,
                ParseErrorKind::MalformedInputError,
                "{query}: {err}"
            );
            assert!(
                err.failure_position.is_some(),
                "{query} should carry a position"
            );
        }
    }

    // ── limits ──────────────────────────────────────────────────────────

    #[test]
    fn oversized_queries_are_limit_errors() {
        let long = format!("name = str('{}')", "a".repeat(limits::MAX_QUERY_BYTES));
        assert_eq!(
            parse(&long).unwrap_err().kind,
            ParseErrorKind::QueryLimitError
        );

        let many = (0..=limits::MAX_PREDICATES)
            .map(|index| format!("a{index} = true"))
            .collect::<alloc::vec::Vec<_>>()
            .join(" AND ");
        assert_eq!(
            parse(&many).unwrap_err().kind,
            ParseErrorKind::QueryLimitError
        );

        // Deep nesting is bounded, so neither the parser nor the evaluator can
        // be driven into unbounded recursion from an RPC call.
        let depth = limits::MAX_NESTING_DEPTH + 1;
        let deep = format!("{}a = true{}", "(".repeat(depth), ")".repeat(depth));
        assert_eq!(
            parse(&deep).unwrap_err().kind,
            ParseErrorKind::QueryLimitError
        );
        let nots = format!("{}a = true", "NOT ".repeat(depth));
        assert_eq!(
            parse(&nots).unwrap_err().kind,
            ParseErrorKind::QueryLimitError
        );
    }

    /// The spec's worked example, with its elided hex filled in.
    #[test]
    fn the_specs_example_query_parses() {
        let query = format!(
            "    level       >= i32(10)
             AND balance     >  u256(1000000)
             AND score       BETWEEN dec(3.5) AND dec(5)
             AND name        =  str('Bob')
             AND desc        STARTSWITH str('ab')
             AND parent      =  key({})
             AND hash        =  bytes32({})
             AND flagged     =  true
             AND $owner      =  addr({})
             AND $expiresAt  <  u64(1200000)
             AND exists(reviewedBy)
             AND typeof(age) =  i32",
            word_hex("12"),
            word_hex("45"),
            addr_hex("ab"),
        );
        assert!(parse(&query).is_ok(), "{:?}", parse(&query).unwrap_err());
    }

    // ── attribute names spelled like the language's words ────────────────
    //
    // No name is reserved. A bare keyword is always the keyword, so an attribute
    // named like one is written quoted, `"and"`. A type tag or an `ORDER BY` word
    // is read by position, so it names an attribute bare or quoted alike. Every
    // such word, in every spelling, is walked through every place a name stands:
    // a comparison, `BETWEEN`, `STARTSWITH`, `exists`, `typeof` and `ORDER BY`,
    // under `NOT`, in a group, joined by `AND` and `OR`, and all at once.
    //
    // A query has no `select`: the projection is a JSON map of names beside the
    // query, never lexed, so its names are covered where the RPC is. The words
    // come from `words`, the lists the lexer is held to.

    /// `word` lowercase, uppercase and capitalized, each once: `i32` capitalizes
    /// to its uppercase. The language's words match in any case, but names are
    /// case-sensitive, so each spelling is its own name.
    fn spellings(word: &str) -> Vec<String> {
        let mut capitalized = word.to_string();
        capitalized[..1].make_ascii_uppercase();
        let mut all = vec![word.to_string(), word.to_ascii_uppercase(), capitalized];
        all.dedup();
        all
    }

    fn quoted(name: &str) -> String {
        format!("\"{name}\"")
    }

    fn statement(text: &str) -> Statement {
        parse_statement(text).unwrap_or_else(|err| panic!("`{text}` should parse: {err}"))
    }

    fn filter(text: &str) -> Query {
        parse(text).unwrap_or_else(|err| panic!("`{text}` should parse: {err}"))
    }

    fn is_bool(name: &str, value: bool) -> Query {
        Query::Eq {
            key: user(name),
            value: AttributeValue::Bool(value),
        }
    }

    /// Every query shape `name` can stand in, as text: each comparison under
    /// each kind of value, `BETWEEN`, `STARTSWITH`, the presence operators, a
    /// negation, a group, both connectives, and `ORDER BY` with and without a
    /// filter on the same name. `name` is spliced in as given, bare or quoted.
    fn every_position(name: &str) -> Vec<String> {
        vec![
            format!("{name} = i32(7)"),
            format!("{name} = 7"),
            format!("{name} < u64(7)"),
            format!("{name} <= u256(7)"),
            format!("{name} > dec(7.5)"),
            format!("{name} >= str('x')"),
            format!("{name} = true"),
            format!("{name} = false"),
            format!("{name} BETWEEN 1 AND 5"),
            format!("{name} BETWEEN str('a') AND str('z') AND a = 1"),
            format!("{name} STARTSWITH str('x')"),
            format!("exists({name})"),
            format!("typeof({name}) = str"),
            format!("typeof({name}) = bool"),
            format!("NOT {name} = true"),
            format!("NOT exists({name})"),
            format!("({name} = true)"),
            format!("a = 1 AND {name} = true"),
            format!("{name} = true OR a = 1"),
            format!("(a = 1 OR {name} < 3) AND NOT ({name} = 2)"),
            format!("* ORDER BY {name}:i32"),
            format!("* ORDER BY {name}:str ASC"),
            format!("* ORDER BY {name}:u256 DESC"),
            format!("{name} > 1 ORDER BY {name}:i32 DESC"),
            format!("exists({name}) ORDER BY {name}:str"),
        ]
    }

    /// Every user attribute a statement names: in its predicates, and the one it
    /// sorts by.
    fn user_names(statement: &Statement) -> BTreeSet<String> {
        fn walk(query: &Query, names: &mut BTreeSet<String>) {
            match query {
                Query::All => {}
                Query::Eq { key, .. }
                | Query::Gt { key, .. }
                | Query::Gte { key, .. }
                | Query::Lt { key, .. }
                | Query::Lte { key, .. }
                | Query::Between { key, .. }
                | Query::StartsWith { key, .. } => {
                    if let AnnotKey::User(name) = key {
                        names.insert(name.clone());
                    }
                }
                Query::Exists { name, .. } => {
                    names.insert(name.clone());
                }
                Query::And(operands) | Query::Or(operands) => {
                    for operand in operands {
                        walk(operand, names);
                    }
                }
                Query::Not(inner) => walk(inner, names),
            }
        }
        let mut names = BTreeSet::new();
        walk(&statement.filter, &mut names);
        if let Some(OrderBy {
            key: AnnotKey::User(name),
            ..
        }) = &statement.order
        {
            names.insert(name.clone());
        }
        names
    }

    // ── every word, every spelling, every position ──────────────────────────

    /// Quoted, every word names an attribute wherever one stands, and the name
    /// is the word exactly as written between the quotes.
    #[test]
    fn every_word_quoted_names_an_attribute_in_every_position() {
        for word in words::all() {
            for spelling in spellings(word) {
                for text in every_position(&quoted(&spelling)) {
                    let mut expected = BTreeSet::from([spelling.clone()]);
                    if text.contains("a = 1") {
                        expected.insert("a".to_string());
                    }
                    assert_eq!(user_names(&statement(&text)), expected, "{text}");
                }
            }
        }
    }

    /// Quotes are optional: wherever a word needs none, quoting it changes
    /// nothing, down to the last node of the tree.
    #[test]
    fn quoting_a_name_that_needs_no_quotes_changes_nothing() {
        let names = words::positional().chain(["level", "my.attr-name_2", "set", "andy", "notes"]);
        for word in names {
            for spelling in spellings(word) {
                for (bare, quoted) in every_position(&spelling)
                    .into_iter()
                    .zip(every_position(&quoted(&spelling)))
                {
                    assert_eq!(statement(&bare), statement(&quoted), "{bare}");
                }
            }
        }
    }

    /// Bare, a keyword can only be the keyword. Wherever it stands for a name, the
    /// error is at the keyword and shows the quoted form, in the spelling written.
    #[test]
    fn a_bare_keyword_is_refused_with_its_quoted_form() {
        let around: [(&str, &str); 12] = [
            ("", " = true"),
            ("", " < 3"),
            ("", " BETWEEN 1 AND 5"),
            ("", " STARTSWITH str('x')"),
            ("a = 1 AND ", " = true"),
            ("a = 1 OR ", " = true"),
            ("NOT ", " = true"),
            ("(", " = true)"),
            ("exists(", ")"),
            ("typeof(", ") = str"),
            ("* ORDER BY ", ":i32"),
            ("a = 1 ORDER BY ", ":str DESC"),
        ];
        for word in words::KEYWORDS {
            for spelling in spellings(word) {
                for (before, after) in around {
                    let text = format!("{before}{spelling}{after}");
                    let err = parse_statement(&text).expect_err(&text);
                    assert_eq!(err.kind, ParseErrorKind::MalformedInputError, "{text}");
                    assert_eq!(err.failure_position, Some(before.len()), "{text}: {err}");
                    assert!(err.message.contains(&quoted(&spelling)), "{text}: {err}");
                }
            }
        }
    }

    /// The words match in any case, but a name keeps its own: each spelling of a
    /// word is a different attribute.
    #[test]
    fn a_quoted_name_keeps_its_case() {
        for word in words::all() {
            let queries: Vec<Query> = spellings(word)
                .iter()
                .map(|name| filter(&format!("{} = 1", quoted(name))))
                .collect();
            for (index, query) in queries.iter().enumerate() {
                for other in &queries[index + 1..] {
                    assert_ne!(query, other, "{word}");
                }
            }
        }
        assert_eq!(
            filter("\"Key\" = 1"),
            Query::Eq {
                key: user("Key"),
                value: AttributeValue::Int(1),
            }
        );
    }

    // ── each construct, pinned exactly ──────────────────────────────────────

    /// The connectives' own words as names, under the connectives: the tree is
    /// what the same query over plain names gives.
    #[test]
    fn keyword_names_sit_under_the_connectives_they_spell() {
        assert_eq!(
            filter("\"and\" = true AND \"or\" = false OR NOT \"not\" = true"),
            Query::Or(vec![
                Query::And(vec![is_bool("and", true), is_bool("or", false)]),
                Query::Not(Box::new(is_bool("not", true))),
            ])
        );
        assert_eq!(
            filter("NOT (\"AND\" = true OR \"Or\" = true)"),
            Query::Not(Box::new(Query::Or(vec![
                is_bool("AND", true),
                is_bool("Or", true),
            ])))
        );
    }

    /// `true` and `false` as names, compared with the values they spell.
    #[test]
    fn the_boolean_words_name_attributes_holding_booleans() {
        assert_eq!(filter("\"true\" = true"), is_bool("true", true));
        assert_eq!(filter("\"false\" = FALSE"), is_bool("false", false));
        assert_eq!(
            filter("\"TRUE\" BETWEEN false AND true"),
            Query::Between {
                key: user("TRUE"),
                bounds: ValueRange::new(AttributeValue::Bool(false), AttributeValue::Bool(true))
                    .expect("one type"),
            }
        );
    }

    /// A `BETWEEN` still owns its `AND` when the attribute is named `between`
    /// or `and`, and the conjunction after it is still a conjunction.
    #[test]
    fn between_owns_its_and_whatever_the_attribute_is_named() {
        assert_eq!(
            filter("\"between\" BETWEEN 1 AND 5 AND \"and\" BETWEEN 2 AND 3"),
            Query::And(vec![
                between_ints("between", 1, 5),
                between_ints("and", 2, 3)
            ])
        );
        assert_eq!(
            filter("\"and\" BETWEEN 1 AND 5 OR \"or\" = true"),
            Query::Or(vec![between_ints("and", 1, 5), is_bool("or", true)])
        );
    }

    #[test]
    fn startswith_tests_an_attribute_named_startswith() {
        assert_eq!(
            filter("\"startswith\" STARTSWITH str('s')"),
            Query::StartsWith {
                key: user("startswith"),
                value: AttributeValue::Str("s".into()),
            }
        );
    }

    /// The presence operators take their own words as names, and a type tag both
    /// as the name and as the type asked for.
    #[test]
    fn presence_operators_take_their_own_words_as_names() {
        assert_eq!(
            filter("exists(\"exists\") AND typeof(\"typeof\") = str"),
            Query::And(vec![
                Query::Exists {
                    name: "exists".into(),
                    attr_type: None,
                },
                Query::Exists {
                    name: "typeof".into(),
                    attr_type: Some(AttributeType::Str),
                },
            ])
        );
        assert_eq!(filter("typeof(str) = str"), filter("typeof(\"str\") = str"),);
        assert_eq!(
            filter("typeof(bool) = key"),
            Query::Exists {
                name: "bool".into(),
                attr_type: Some(AttributeType::EntityKey),
            }
        );
    }

    /// A type tag names an attribute bare, and is still a literal's tag after
    /// the operator: `str = str('x')`.
    #[test]
    fn a_type_tag_names_an_attribute_and_tags_its_value() {
        assert_eq!(
            filter("str = str('x')"),
            Query::Eq {
                key: user("str"),
                value: AttributeValue::Str("x".into()),
            }
        );
        assert_eq!(
            filter("u64 >= u64(3)"),
            Query::Gte {
                key: user("u64"),
                value: AttributeValue::U64(3),
            }
        );
        let key = format!("0x{}", "ab".repeat(32));
        assert_eq!(
            filter(&format!("key = key({key}) AND KEY = key({key})")),
            Query::And(vec![
                Query::Eq {
                    key: user("key"),
                    value: AttributeValue::EntityKey([0xab; 32]),
                },
                Query::Eq {
                    key: user("KEY"),
                    value: AttributeValue::EntityKey([0xab; 32]),
                },
            ])
        );
        assert_eq!(filter("bool = true"), is_bool("bool", true));
        // Whitespace before a `(` still makes a tag a literal's, never a name's.
        assert_eq!(filter("dec = dec (3)"), filter("dec = dec(3)"));
    }

    /// `ORDER BY` sorts by a keyword-named attribute quoted, and by a tag-named
    /// or clause-word-named one bare.
    #[test]
    fn order_by_sorts_by_a_name_spelled_like_any_word() {
        let order = |key: &str, attr_type, direction| {
            Some(OrderBy {
                key: user(key),
                attr_type,
                direction,
            })
        };
        assert_eq!(
            statement("\"desc\" > 1 ORDER BY \"desc\":i32 DESC"),
            Statement {
                filter: Query::Gt {
                    key: user("desc"),
                    value: AttributeValue::Int(1),
                },
                order: order("desc", AttributeType::Int, Direction::Descending),
            }
        );
        for (text, key, attr_type, direction) in [
            (
                "* ORDER BY \"and\":str",
                "and",
                AttributeType::Str,
                Direction::Ascending,
            ),
            (
                "* ORDER BY \"NOT\":u64 desc",
                "NOT",
                AttributeType::U64,
                Direction::Descending,
            ),
            (
                "* ORDER BY str:str",
                "str",
                AttributeType::Str,
                Direction::Ascending,
            ),
            (
                "* ORDER BY Key:key DESC",
                "Key",
                AttributeType::EntityKey,
                Direction::Descending,
            ),
            (
                "* ORDER BY desc:i32 asc",
                "desc",
                AttributeType::Int,
                Direction::Ascending,
            ),
            (
                "* ORDER BY asc:i32 desc",
                "asc",
                AttributeType::Int,
                Direction::Descending,
            ),
            (
                "* ORDER BY order:bool",
                "order",
                AttributeType::Bool,
                Direction::Ascending,
            ),
            (
                "* ORDER BY \"by\":dec",
                "by",
                AttributeType::Decimal,
                Direction::Ascending,
            ),
        ] {
            assert_eq!(
                statement(text).order,
                order(key, attr_type, direction),
                "{text}"
            );
        }
    }

    // ── quotes make a name, and only a name ─────────────────────────────────

    /// A quoted word is never a word of the clause, so it cannot open, continue
    /// or end one, but it can stand anywhere a name does.
    #[test]
    fn a_quoted_clause_word_is_a_name_not_the_clause() {
        assert_eq!(
            filter("\"order\" = 1 AND \"ORDER\" = 2"),
            Query::And(vec![
                Query::Eq {
                    key: user("order"),
                    value: AttributeValue::Int(1),
                },
                Query::Eq {
                    key: user("ORDER"),
                    value: AttributeValue::Int(2),
                },
            ])
        );
        for text in [
            "* \"ORDER\" BY a:i32",
            "* ORDER \"BY\" a:i32",
            "* ORDER BY a:i32 \"DESC\"",
            "* ORDER BY a:i32 \"asc\"",
            "a = 1 \"order\" BY a:i32",
        ] {
            let err = parse_statement(text).expect_err(text);
            assert_eq!(err.kind, ParseErrorKind::MalformedInputError, "{text}");
        }
    }

    /// Quotes make a name, so a quoted tag is never a tag: not a literal's, not
    /// `typeof`'s and not the type `ORDER BY` sorts as.
    #[test]
    fn a_quoted_tag_is_never_a_tag() {
        for (text, hint) in [
            ("typeof(a) = \"str\"", "typeof(name) = str"),
            ("typeof(a) = \"rank\"", "typeof(name) = i32"),
            ("* ORDER BY a:\"u256\"", "ORDER BY name:u256"),
        ] {
            let err = parse_statement(text).expect_err(text);
            assert_eq!(err.kind, ParseErrorKind::MalformedInputError, "{text}");
            assert!(err.message.contains(hint), "{text}: {err}");
        }
        assert!(parse("a = \"str\"('x')").is_err());
    }

    /// Double quotes make a name, never a string, and the error says how a
    /// string is written.
    #[test]
    fn a_double_quoted_value_is_refused_with_the_string_form() {
        let err = parse("name = \"Bob\"").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::MalformedInputError);
        assert_eq!(err.failure_position, Some("name = ".len()));
        assert!(err.message.contains("str('Bob')"), "{err}");
    }

    /// A quoted name is held to the same rules as a bare one: a bare name's
    /// characters, at most 32 bytes, and never a system attribute.
    #[test]
    fn a_quoted_name_obeys_the_name_rules() {
        let at_limit = "a".repeat(limits::MAX_ATTRIBUTE_NAME_BYTES);
        assert_eq!(
            filter(&format!("{} = true", quoted(&at_limit))),
            is_bool(&at_limit, true)
        );
        let over = "a".repeat(limits::MAX_ATTRIBUTE_NAME_BYTES + 1);
        let err = parse(&format!("{} = true", quoted(&over))).unwrap_err();
        assert!(err.message.contains("32 bytes"), "{err}");

        for text in [
            "\"\" = true",
            "\"a b\" = true",
            "\"1a\" = true",
            "\"$owner\" = addr(0x0000000000000000000000000000000000000000)",
            "\"and = true",
            "\"a\"\"b\" = true",
            "a\"b\" = true",
            "exists(\"\")",
            "* ORDER BY \"\":i32",
        ] {
            let err = parse_statement(text).expect_err(text);
            assert_eq!(err.kind, ParseErrorKind::MalformedInputError, "{text}");
        }
    }

    // ── all of them at once ─────────────────────────────────────────────────

    /// One query naming an attribute after every word the language has, in every
    /// construct, joined every way, and sorted by a keyword-named attribute.
    #[test]
    fn one_query_names_every_word_at_once() {
        let text = "\
            (\"and\" = true OR \"or\" = false OR NOT \"not\" = true)
            AND \"true\" BETWEEN false AND true
            AND \"false\" = false
            AND \"between\" BETWEEN 1 AND 5
            AND \"startswith\" STARTSWITH str('s')
            AND exists(\"exists\")
            AND typeof(\"typeof\") = str
            AND (i32 >= i32(-1) OR u64 < u64(9) OR \"u256\" > u256(0x10))
            AND NOT (dec = dec(1.5) OR str = str('x'))
            AND addr = addr(0x0000000000000000000000000000000000000001)
            AND \"key\" = key(0x0000000000000000000000000000000000000000000000000000000000000002)
            AND bytes32 = bytes32(0x0000000000000000000000000000000000000000000000000000000000000003)
            AND typeof(bool) = bool
            AND (order = 1 OR \"by\" = 2)
            AND asc STARTSWITH str('a')
            AND exists(desc)
            ORDER BY \"and\":bool DESC";
        let parsed = statement(text);

        assert_eq!(
            user_names(&parsed),
            words::all().map(String::from).collect::<BTreeSet<_>>()
        );
        assert_eq!(
            parsed.order,
            Some(OrderBy {
                key: user("and"),
                attr_type: AttributeType::Bool,
                direction: Direction::Descending,
            })
        );
        let Query::And(operands) = &parsed.filter else {
            panic!("a conjunction at the top: {:?}", parsed.filter);
        };
        assert_eq!(operands.len(), 16);
        assert_eq!(
            operands[0],
            Query::Or(vec![
                is_bool("and", true),
                is_bool("or", false),
                Query::Not(Box::new(is_bool("not", true))),
            ])
        );
        assert_eq!(operands[3], between_ints("between", 1, 5));
        assert_eq!(
            operands[13],
            Query::Or(vec![
                Query::Eq {
                    key: user("order"),
                    value: AttributeValue::Int(1),
                },
                Query::Eq {
                    key: user("by"),
                    value: AttributeValue::Int(2),
                },
            ])
        );
        // Quoting the names written bare gives the same statement. The keywords
        // are quoted already, and their words also stand bare as values.
        let all_quoted = words::positional().fold(text.to_string(), |text, word| {
            text.replace(&format!(" {word} "), &format!(" \"{word}\" "))
                .replace(&format!("({word} "), &format!("(\"{word}\" "))
                .replace(&format!("({word})"), &format!("(\"{word}\")"))
        });
        assert_ne!(all_quoted, text);
        assert_eq!(statement(&all_quoted), parsed);
    }
}
