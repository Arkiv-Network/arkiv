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
//! AND score       >= dec(3.5)
//! AND name        =  str('Bob')
//! AND desc        STARTSWITH str('ab')
//! AND parent      =  key(0x123…)
//! AND flagged     =  true
//! AND $owner      =  addr(0xAbC…)
//! AND $expiresAt  <  1200000        -- system attributes take bare literals
//! ```
//!
//! Every user value carries a **type tag**, and that tag is part of what the
//! predicate asserts: `level >= i32(10)` matches only entities whose `level` is
//! an `i32`, never a `u256` that happens to hold 10. Booleans are the one bare
//! form (`true` / `false`); system attributes take untagged literals because
//! their types are protocol-fixed, though tags are accepted where they exist.
//!
//! Ordering is only defined for the numeric types, so `< <= > >=` on anything
//! else is a parse error rather than an empty result. `STARTSWITH` is the only
//! pattern operator: a raw UTF-8 byte prefix, no normalization.
//!
//! ## What this version leaves out
//!
//! `exists(…)`, `typeof(…)` and `!=` are **reserved but not implemented**. All
//! three need a per-`(attribute, type)` presence index the host does not
//! maintain, and answering them from what exists would silently return the wider
//! `NOT` complement instead. They parse to a directive error naming the
//! alternative; `NOT (attr = value)` gives the complement explicitly. The
//! reasoning is written up in `docs/v1-typed-query-scope.md`.
//!
//! ```
//! use arkiv_query::parse;
//! use arkiv_interfaces::query::Query;
//!
//! assert_eq!(parse("*").unwrap(), Query::All);
//! assert!(parse("level >= i32(10) AND name = str('Bob')").is_ok());
//!
//! // A range operator on an unordered type is rejected, not answered emptily.
//! assert!(parse("owner.tag >= str('x')").is_err());
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
pub use parse::parse;
