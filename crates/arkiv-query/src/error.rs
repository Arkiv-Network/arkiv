//! Why a query string was rejected.
//!
//! The [`kind`](ParseError::kind) is part of the node's public surface: it maps
//! one-to-one onto the JSON-RPC error codes the spec freezes
//! ([`rpc_error_codes`]), so a client can act on the failure without reading the
//! message.

use alloc::string::{String, ToString};
use arkiv_interfaces::constants::rpc_error_codes;
use core::fmt;

/// What sort of failure this is — and, for a node, which RPC code to answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Malformed input: an unexpected token, an unclosed group, trailing junk.
    ///
    /// Reported as [`rpc_error_codes::MALFORMED_INPUT`].
    MalformedInputError,

    /// Well-formed but not well-typed: a range operator on an equality-only type,
    /// an unknown type tag, a value whose type doesn't fit the attribute.
    ///
    /// Reported as [`rpc_error_codes::TYPE_ERROR`].
    TypeError,

    /// A literal that doesn't fit its tag: `i32` out of range, a bad EIP-55
    /// checksum, more than 18 decimal places, an over-long string.
    ///
    /// Reported as [`rpc_error_codes::LITERAL_ERROR`].
    LiteralError,

    /// The query is too big: too long, too many predicates, nested too deeply.
    ///
    /// Reported as [`rpc_error_codes::QUERY_LIMIT`].
    QueryLimitError,
}

impl ParseErrorKind {
    /// The JSON-RPC error code this kind answers with.
    pub const fn rpc_code(self) -> i32 {
        match self {
            Self::MalformedInputError => rpc_error_codes::MALFORMED_INPUT,
            Self::TypeError => rpc_error_codes::TYPE_ERROR,
            Self::LiteralError => rpc_error_codes::LITERAL_ERROR,
            Self::QueryLimitError => rpc_error_codes::QUERY_LIMIT,
        }
    }
}

/// Description of a query-language failure, including its position in the string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub message: String,
    pub failure_position: Option<usize>,
}

impl ParseError {
    /// An error at a known byte offset.
    pub(crate) fn at(
        failure_position: usize,
        kind: ParseErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            failure_position: Some(failure_position),
        }
    }

    /// An error with no meaningful position (a whole-query limit, say).
    pub(crate) fn whole(kind: ParseErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            failure_position: None,
        }
    }

    /// A syntax error at `position`.
    pub(crate) fn syntax(failure_position: usize, message: impl Into<String>) -> Self {
        Self::at(
            failure_position,
            ParseErrorKind::MalformedInputError,
            message,
        )
    }

    /// A type error at `position`.
    pub(crate) fn type_error(failure_position: usize, message: impl Into<String>) -> Self {
        Self::at(failure_position, ParseErrorKind::TypeError, message)
    }

    /// A literal-validation error at `position`.
    pub(crate) fn literal(failure_position: usize, message: impl Into<String>) -> Self {
        Self::at(failure_position, ParseErrorKind::LiteralError, message)
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failure_position {
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
