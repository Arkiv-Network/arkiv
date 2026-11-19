//! The words of the query language: every spelling its grammar gives a
//! meaning to, as `docs/api-spec.md` §4 lists them.
//!
//! None of them is reserved as an attribute name. A bare keyword is always
//! the keyword, so a query names an attribute after one quoted, `"and"`. A
//! type tag or an `ORDER BY` word is read only where it stands, so it names an
//! attribute bare too, `str = str('x')`. The words match in any case; names
//! keep theirs.

/// The keywords.
pub const KEYWORDS: [&str; 9] = [
    "and",
    "or",
    "not",
    "true",
    "false",
    "between",
    "startswith",
    "exists",
    "typeof",
];

/// The type tags: a literal's (`i32(10)`), `typeof`'s right-hand side, and
/// the type an `ORDER BY` attribute sorts as.
pub const TYPE_TAGS: [&str; 9] = [
    "i32", "u64", "u256", "dec", "str", "addr", "key", "bytes32", "bool",
];

/// The words of the `ORDER BY` clause.
pub const ORDER_BY: [&str; 4] = ["order", "by", "asc", "desc"];

/// Every word: the keywords, then the type tags, then the `ORDER BY` words.
pub fn all() -> impl Iterator<Item = &'static str> {
    KEYWORDS.iter().copied().chain(positional())
}

/// The words read by position, which name an attribute bare as well as
/// quoted: the type tags, then the `ORDER BY` words.
pub fn positional() -> impl Iterator<Item = &'static str> {
    TYPE_TAGS.iter().chain(ORDER_BY.iter()).copied()
}

/// Test scaffolding: what an attribute named after each word holds, for tests
/// that store every word as an attribute and query it back. The words hold
/// `u64`, `str` and `i32` in turn, in [`all`]'s order, so between them they
/// cover both literal shapes, `u64(2)` and `str('v2')`.
#[cfg(feature = "conformance")]
pub mod sample {
    use alloc::format;
    use alloc::string::String;

    use super::all;
    use crate::entity::{AttributeType, AttributeValue};

    /// The value the attribute named `word` holds on the `n`th entity: `n`,
    /// or `v{n}` as a `str`.
    ///
    /// # Panics
    ///
    /// When `word` is not one of [`all`].
    pub fn value(word: &str, n: u8) -> AttributeValue {
        let index = all()
            .position(|known| known == word)
            .unwrap_or_else(|| panic!("{word} is not a word of the query language"));
        match index % 3 {
            0 => AttributeValue::U64(u64::from(n)),
            1 => AttributeValue::Str(format!("v{n}")),
            _ => AttributeValue::Int(i32::from(n)),
        }
    }

    /// The type the attribute named `word` holds.
    pub fn attr_type(word: &str) -> AttributeType {
        value(word, 0).attr_type()
    }

    /// [`value`] as a query literal.
    pub fn literal(word: &str, n: u8) -> String {
        match value(word, n) {
            AttributeValue::Str(text) => format!("str('{text}')"),
            other => format!("{}({n})", other.attr_type().name()),
        }
    }
}
