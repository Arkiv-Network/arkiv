//! The auxiliary store's error type.

use crate::indices::bitmap::BitmapError;

/// Something went wrong reading or writing the index.
///
/// Generic over the backend's own error `E` (an
/// [`AccountCode`](crate::entities::AccountCode) or
/// [`IndexStorage`](crate::indices::storage::IndexStorage) failure), plus the one error the
/// index logic itself can raise: a stored pair bitmap whose bytes won't deserialize.
#[derive(Debug)]
pub enum AuxError<E> {
    /// A backend read or write failed.
    Backend(E),
    /// A tier-1 pair account held bytes that aren't a valid bitmap — a corrupt or
    /// forked index.
    Bitmap(BitmapError),
    /// A range lookup with no typed bound on either side — nothing names the
    /// `(attribute, type)` buckets to scan, so the request is unanswerable.
    UnboundedRange,
    /// A bulk insert-only fold was handed a delta that removes annotations —
    /// see [`RethAuxStore::apply_inserts_bulk`](crate::RethAuxStore::apply_inserts_bulk).
    BulkRemovesUnsupported,
}

impl<E> From<BitmapError> for AuxError<E> {
    fn from(error: BitmapError) -> Self {
        AuxError::Bitmap(error)
    }
}
