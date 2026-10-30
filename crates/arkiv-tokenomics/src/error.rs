//! The one way pricing fails.

/// A value left the range its type holds: a sum or product too large, or a
/// lifetime that has already ended. Pricing inputs are consensus, so this is
/// never saturated or wrapped; the transaction fails instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overflow;
