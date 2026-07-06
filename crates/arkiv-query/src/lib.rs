//! The **Arkiv query language** — parse a query string into the
//! [`arkiv_interfaces::query::Query`] AST.
//!
//! The grammar is part of the Arkiv specification (it defines how you query the
//! database), and the AST it targets lives in `arkiv-interfaces`. Parsing itself
//! is *logic*, so it lives here instead — a hand-rolled lexer + recursive-descent
//! parser, kept `#![no_std]` and zero-external-dep (only the AST) so it stays
//! deterministic and portable (fault-proof / zkVM friendly).
//!
//! Parsing produces a **typed** AST: [`AnnotVal`](arkiv_interfaces::query::AnnotVal)
//! carries `Uint`/`Str`/`Key`/`Addr`, not any host's index bytes. Turning those
//! values into a particular index layout is the host's job, in the evaluator.
//!
//! ```
//! use arkiv_query::parse;
//! use arkiv_interfaces::query::Query;
//! assert_eq!(parse("*").unwrap(), Query::All);
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod lexer;
pub mod parse;

pub use parse::{ParseError, parse};
