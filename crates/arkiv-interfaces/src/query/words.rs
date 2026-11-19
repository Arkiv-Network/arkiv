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
