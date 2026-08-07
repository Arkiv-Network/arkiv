//! Why a query string was rejected.
//!
//! The [`kind`](ParseError::kind) is part of the node's public surface: it maps
//! one-to-one onto the JSON-RPC error codes the spec freezes, so a client can act
//! on the failure without reading the message.

use alloc::string::{String, ToString};
use core::fmt;

/// What sort of failure this is — and, for a node, which RPC code to answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Malformed input: an unexpected token, an unclosed group, trailing junk.
    /// RPC `-32001`.
    Syntax,
    /// Well-formed but not well-typed: a range operator on an equality-only type,
    /// an unknown type tag, a value whose type doesn't fit the attribute.
    /// RPC `-32002`.
    Type,
    /// A literal that doesn't fit its tag: `i32` out of range, a bad EIP-55
    /// checksum, more than 18 decimal places, an over-long string.
    /// RPC `-32003`.
    Literal,
    /// The query is too big: too long, too many predicates, nested too deeply.
    /// RPC `-32004`.
    Limit,
}

impl ParseErrorKind {
    /// The JSON-RPC error code this kind answers with.
    pub const fn rpc_code(self) -> i32 {
        match self {
            Self::Syntax => -32001,
            Self::Type => -32002,
            Self::Literal => -32003,
            Self::Limit => -32004,
        }
    }
}

/// Why a query string failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Which class of failure — see [`ParseErrorKind`].
    pub kind: ParseErrorKind,
    /// Human-readable description. Written to be actionable: it names the fix
    /// ("use i32(10)") rather than only the problem.
    pub message: String,
    /// Byte offset into the input where the failure was detected, when known.
    pub position: Option<usize>,
}

impl ParseError {
    /// An error at a known byte offset.
    pub(crate) fn at(position: usize, kind: ParseErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            position: Some(position),
        }
    }

    /// An error with no meaningful position (a whole-query limit, say).
    pub(crate) fn whole(kind: ParseErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            position: None,
        }
    }

    /// A syntax error at `position`.
    pub(crate) fn syntax(position: usize, message: impl Into<String>) -> Self {
        Self::at(position, ParseErrorKind::Syntax, message)
    }

    /// A type error at `position`.
    pub(crate) fn type_error(position: usize, message: impl Into<String>) -> Self {
        Self::at(position, ParseErrorKind::Type, message)
    }

    /// A literal-validation error at `position`.
    pub(crate) fn literal(position: usize, message: impl Into<String>) -> Self {
        Self::at(position, ParseErrorKind::Literal, message)
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.position {
            Some(p) => write!(f, "query error at byte {p}: {}", self.message),
            None => write!(f, "query error: {}", self.message),
        }
    }
}

impl core::error::Error for ParseError {}

/// Shorthand so `?` on an `Option` reads well in the literal parsers.
pub(crate) fn literal_err<T>(position: usize, message: &str) -> Result<T, ParseError> {
    Err(ParseError::literal(position, message.to_string()))
}
