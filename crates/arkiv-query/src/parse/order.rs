//! The `ORDER BY` clause, which may end a query:
//!
//! ```text
//! orderBy → ORDER BY attrRef [ ':' typeTag ] [ ASC | DESC ]
//! ```
//!
//! Its words are read where the clause stands, not reserved, so `order`,
//! `by`, `asc` and `desc` stay attribute names, as `desc` is in the spec's
//! example query. After the predicates the only other thing that may follow
//! is `AND` or `OR`, so an `order` there can only open the clause.
//!
//! The clause sorts by one attribute read as one type. A user attribute can
//! hold any type, so it names the one after a `:`, as in `rank:u256`; a system
//! attribute holds one, which a `:` may repeat but not contradict.

use alloc::format;

use arkiv_interfaces::entity::AttributeType;
use arkiv_interfaces::query::{AnnotKey, Direction, OrderBy};

use super::{Parser, TagSite, builtin_type};
use crate::error::ParseError;
use crate::lexer::Token;

impl Parser {
    /// The clause, from its `ORDER` to the end of the query.
    pub(super) fn parse_order_by(&mut self) -> Result<OrderBy, ParseError> {
        let order_at = self.next_position();
        self.advance();
        if self.word_at("by").is_none() {
            return Err(ParseError::syntax(
                order_at,
                "ORDER must be followed by BY, as in ORDER BY name:str — to test an \
                 attribute named order, join it with AND or OR",
            ));
        }
        self.advance();
        let key_at = self.next_position();
        let key = self.parse_attr_ref()?;
        let attr_type = self.parse_sort_type(&key, key_at)?;
        let direction = self.parse_direction();
        if let Some(spanned) = self.peek() {
            return Err(ParseError::syntax(
                spanned.start,
                "unexpected input after ORDER BY — it ends the query, and sorts by one \
                 attribute",
            ));
        }
        Ok(OrderBy {
            key,
            attr_type,
            direction,
        })
    }

    /// The type `key` is sorted as: the one after its `:`, or the one a system
    /// attribute holds.
    fn parse_sort_type(
        &mut self,
        key: &AnnotKey,
        key_at: usize,
    ) -> Result<AttributeType, ParseError> {
        let named = match self.peek_token() {
            Some(Token::Colon) => {
                self.advance();
                let tag_at = self.next_position();
                Some((self.parse_type_tag(TagSite::OrderBy)?, tag_at))
            }
            _ => None,
        };
        match key {
            AnnotKey::User(name) => named.map(|(attr_type, _)| attr_type).ok_or_else(|| {
                ParseError::type_error(
                    key_at,
                    format!(
                        "{name} can hold any type, so ORDER BY names the one to sort it \
                         as — write ORDER BY {name}:str, or another tag"
                    ),
                )
            }),
            AnnotKey::BuiltIn(field) => {
                let held = builtin_type(*field);
                match named {
                    Some((attr_type, tag_at)) if attr_type != held => Err(ParseError::type_error(
                        tag_at,
                        format!(
                            "{} holds {}, so it sorts as {} — not as {}",
                            key.cell_name(),
                            held.name(),
                            held.name(),
                            attr_type.name(),
                        ),
                    )),
                    Some(_) | None => Ok(held),
                }
            }
        }
    }

    /// `ASC` or `DESC`, ascending when neither is written.
    fn parse_direction(&mut self) -> Direction {
        let direction = if self.word_at("desc").is_some() {
            Direction::Descending
        } else if self.word_at("asc").is_some() {
            Direction::Ascending
        } else {
            return Direction::Ascending;
        };
        self.advance();
        direction
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use arkiv_interfaces::entity::AttributeValue;
    use arkiv_interfaces::query::{BuiltIn, Query, Statement};

    use super::*;
    use crate::error::ParseErrorKind;
    use crate::parse::{parse, parse_statement};

    fn user(name: &str) -> AnnotKey {
        AnnotKey::User(name.to_string())
    }

    fn order(key: AnnotKey, attr_type: AttributeType, direction: Direction) -> Option<OrderBy> {
        Some(OrderBy {
            key,
            attr_type,
            direction,
        })
    }

    /// The order a whole query string parses to.
    fn order_of(text: &str) -> Option<OrderBy> {
        parse_statement(text)
            .unwrap_or_else(|err| panic!("{text}: {err}"))
            .order
    }

    fn err_of(text: &str) -> ParseError {
        parse_statement(text).expect_err(text)
    }

    #[test]
    fn a_query_ends_with_its_order() {
        assert_eq!(
            parse_statement("rank >= u256(10) ORDER BY rank:u256 DESC").unwrap(),
            Statement {
                filter: Query::Gte {
                    key: user("rank"),
                    value: AttributeValue::u256_from_u64(10),
                },
                order: order(user("rank"), AttributeType::U256, Direction::Descending),
            }
        );
        // `*` takes an order too.
        assert_eq!(
            parse_statement("* ORDER BY name:str").unwrap(),
            Statement {
                filter: Query::All,
                order: order(user("name"), AttributeType::Str, Direction::Ascending),
            }
        );
    }

    /// Whitespace around the `:` is insignificant, as everywhere else.
    #[test]
    fn ascending_unless_desc_and_every_word_in_any_case() {
        for (text, direction) in [
            ("* ORDER BY n:u64", Direction::Ascending),
            ("* ORDER BY n:u64 ASC", Direction::Ascending),
            ("* ORDER BY n : u64", Direction::Ascending),
            ("* order by n:u64 desc", Direction::Descending),
            ("* Order By n:U64 Desc", Direction::Descending),
        ] {
            assert_eq!(
                order_of(text),
                order(user("n"), AttributeType::U64, direction),
                "{text}"
            );
        }
    }

    /// The words are read where the clause stands, so they remain names
    /// everywhere else, and even as the attribute the clause sorts by.
    #[test]
    fn the_clause_reserves_none_of_its_words() {
        let statement =
            parse_statement("desc STARTSWITH str('a') AND order = 1 ORDER BY desc:str DESC")
                .unwrap();
        assert!(matches!(statement.filter, Query::And(_)));
        assert_eq!(
            statement.order,
            order(user("desc"), AttributeType::Str, Direction::Descending)
        );
        for name in ["order", "by", "asc", "desc"] {
            assert_eq!(
                order_of(&alloc::format!("* ORDER BY {name}:i32")),
                order(user(name), AttributeType::Int, Direction::Ascending),
                "{name}"
            );
        }
    }

    /// A user attribute can hold any type, so the clause names the one.
    #[test]
    fn a_user_attribute_needs_its_type() {
        for text in ["* ORDER BY rank", "* ORDER BY rank AS str"] {
            let err = err_of(text);
            assert_eq!(err.kind, ParseErrorKind::TypeError, "{text}");
            assert!(err.message.contains("ORDER BY rank:str"), "{err}");
            assert_eq!(err.failure_position, Some("* ORDER BY ".len()), "{text}");
        }
    }

    /// A system attribute holds one type, which a `:` may repeat but not
    /// contradict.
    #[test]
    fn a_system_attribute_sorts_as_the_type_it_holds() {
        for (text, field, attr_type) in [
            (
                "* ORDER BY $expiresAt",
                BuiltIn::ExpiresAt,
                AttributeType::U64,
            ),
            (
                "* ORDER BY $expiresAt:u64",
                BuiltIn::ExpiresAt,
                AttributeType::U64,
            ),
            (
                "* ORDER BY $owner",
                BuiltIn::Owner,
                AttributeType::EthereumAddress,
            ),
            (
                "* ORDER BY $creator:addr",
                BuiltIn::Creator,
                AttributeType::EthereumAddress,
            ),
            ("* ORDER BY $key", BuiltIn::Key, AttributeType::EntityKey),
        ] {
            assert_eq!(
                order_of(text),
                order(AnnotKey::BuiltIn(field), attr_type, Direction::Ascending),
                "{text}"
            );
        }
        let err = err_of("* ORDER BY $owner:u64");
        assert_eq!(err.kind, ParseErrorKind::TypeError);
        assert!(err.message.contains("$owner holds addr"), "{err}");
        assert_eq!(err.failure_position, Some("* ORDER BY $owner:".len()));
        // One that is not queryable cannot be sorted by either.
        assert_eq!(
            err_of("* ORDER BY $createdAt").kind,
            ParseErrorKind::TypeError
        );
    }

    #[test]
    fn the_colon_takes_a_bare_type_tag() {
        let err = err_of("* ORDER BY a:bytes");
        assert_eq!(err.kind, ParseErrorKind::TypeError);
        assert!(err.message.contains("not a type tag"), "{err}");

        let err = err_of("* ORDER BY a:str('x')");
        assert_eq!(err.kind, ParseErrorKind::TypeError);
        assert!(err.message.contains("ORDER BY name:str"), "{err}");
    }

    #[test]
    fn the_clause_needs_its_full_shape_and_ends_the_query() {
        for text in [
            "* ORDER",
            "* ORDER rank:str",
            "* ORDER BY",
            "* ORDER BY 5:i32",
            "* ORDER BY rank:",
            "* ORDER BY rank::str",
            "* ORDER BY rank:str:u64",
            "* ORDER BY rank:str sideways",
            "* ORDER BY rank:str DESC rank",
            "* ORDER BY rank:str AND a = 1",
            "* ORDER BY $expiresAt AS u64",
            "(a = 1 ORDER BY a:i32)",
        ] {
            assert_eq!(
                err_of(text).kind,
                ParseErrorKind::MalformedInputError,
                "`{text}` should not parse"
            );
        }
    }

    /// `parse` reads the predicates alone, and says so of an order.
    #[test]
    fn parse_refuses_an_order_and_parse_statement_takes_none() {
        let err = parse("* ORDER BY name:str").unwrap_err();
        assert_eq!(err.kind, ParseErrorKind::MalformedInputError);
        assert_eq!(err.failure_position, Some("* ".len()));

        let text = "a = 1 OR b = 2";
        assert_eq!(
            parse_statement(text).unwrap(),
            Statement {
                filter: parse(text).unwrap(),
                order: None,
            }
        );
    }
}
