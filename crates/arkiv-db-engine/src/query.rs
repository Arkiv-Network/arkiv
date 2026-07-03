//! Arkiv query language: lexer + parser + interpreter.
//!
//! Three modules, one per pipeline stage:
//!
//! - [`lexer`] — tokenize a query string into [`lexer::Token`]s.
//! - [`parser`] — parse a token stream into a [`parser::Query`] AST.
//! - `interpreter` (Phase 10b) — evaluate the AST against a
//!   [`crate::StateAdapter`] to a [`crate::Bitmap`] of matching entity IDs.
//!
//! The public surface re-exported here is the only thing callers
//! outside `arkiv_db_engine` should depend on.

pub mod interpreter;
pub mod lexer;
pub mod parser;

pub use interpreter::{Page, PageParams, execute};
pub use parser::{AnnotKey, AnnotVal, BuiltIn, Query, parse};
