//! The **Arkiv query language** — parse a query string into the
//! [`arkiv_interfaces::query::Query`] AST.
//!
//! The grammar is part of the Arkiv specification (it defines how you query the
//! database), and the AST it targets lives in `arkiv-interfaces`. Parsing itself
//! is *logic*, so it lives here — a hand-rolled lexer plus a recursive-descent
//! parser, kept `#![no_std]` and dependency-light so it stays deterministic and
//! portable (fault-proof / zkVM friendly).
//!
//! ## The language
//!
//! A query is a bare boolean expression — no `WHERE`. Keywords are
//! case-insensitive and `--` comments run to end of line:
//!
//! ```text
//!     level       >= i32(10)
//! AND balance     >  u256(1000000)
//! AND score       BETWEEN dec(3.5) AND dec(5)
//! AND name        =  str('Bob')
//! AND desc        STARTSWITH str('ab')
//! AND parent      =  key(0x123…)
//! AND flagged     =  true
//! AND $owner      =  addr(0xAbC…)
//! AND $expiresAt  <  u64(1200000)   -- system u64: explicitly tagged
//! ```
//!
//! Every value carries a **type tag**, and that tag is part of what the
//! predicate asserts: `level >= i32(10)` matches only entities whose `level` is
//! an `i32`, never a `u256` that happens to hold 10.
//!
//! Two things go untagged, and only two. A boolean is bare (`true` / `false`),
//! and an untagged **number means `i32`** — so `level >= 10` is exactly
//! `level >= i32(10)`. Everything else must be spelled out, *including* the
//! system block heights, which are `u64`: `$expiresAt < u64(1200000)`. That is
//! deliberate — if a bare number meant different types in different places, you
//! could not read a predicate and know what it asserts.
//!
//! Every type the language can write is ordered, so `< <= > >=` take any of
//! them: numerically for `i32`, `u64`, `u256` and `dec`, `false < true` for a
//! `bool`, and unsigned byte order for `str`, `bytes32`, `addr` and `key`.
//! `STARTSWITH` is the only pattern operator: a raw UTF-8 byte prefix, no
//! normalization, and on anything but a `str` a parse error rather than an
//! empty result.
//!
//! `a BETWEEN lo AND hi` matches what `a >= lo AND a <= hi` does: both bounds
//! included, of one type, and nothing at all when `lo` is above `hi`. The
//! `AND` inside it is its own, so `a BETWEEN 1 AND 5 AND b = 2` is
//! `(a BETWEEN 1 AND 5) AND b = 2`.
//!
//! ## Sorting
//!
//! A query may end with `ORDER BY attr [AS tag] [ASC | DESC]`, which sorts its
//! matches by one attribute read as one type: a user attribute names the
//! type, as every value does, and a system attribute sorts as the one it
//! holds. `ASC` is the default. [`parse_statement`] reads a whole query
//! string, the clause included, into a
//! [`Statement`](arkiv_interfaces::query::Statement); [`parse()`] reads the
//! predicates alone and refuses the clause.
//!
//! ```
//! use arkiv_query::parse_statement;
//! use arkiv_interfaces::query::Direction;
//!
//! let statement = parse_statement("rank >= u256(10) ORDER BY rank AS u256 DESC").unwrap();
//! assert_eq!(statement.order.unwrap().direction, Direction::Descending);
//!
//! // A user attribute can hold any type, so the clause has to name one.
//! assert!(parse_statement("* ORDER BY rank").is_err());
//! ```
//!
//! ## What this version leaves out
//!
//! `!=` is **reserved but not implemented**: typed value-negation is
//! `exists(attr)` minus `attr = value`, which no single store predicate
//! expresses, and answering it from what exists would silently return the
//! wider `NOT` complement instead. It parses to a directive error naming the
//! alternative; `NOT (attr = value)` gives the complement explicitly.
//!
//! `exists(…)` and `typeof(…)` **are** implemented — see the presence
//! operators above.
//!
//! ```
//! use arkiv_query::parse;
//! use arkiv_interfaces::query::Query;
//!
//! assert_eq!(parse("*").unwrap(), Query::All);
//! assert!(parse("level >= i32(10) AND name = str('Bob')").is_ok());
//!
//! // A prefix on anything but a string is rejected, not answered emptily.
//! assert!(parse("level STARTSWITH i32(1)").is_err());
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod error;
mod lexer;
pub mod limits;
mod literal;
pub mod parse;

pub use error::{ParseError, ParseErrorKind};
pub use literal::MAX_STR_BYTES;
pub use parse::{parse, parse_statement};
