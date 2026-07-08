//! The auxiliary store's error type.

use crate::bitmap::BitmapError;

/// Something went wrong reading or writing the index.
///
/// Generic over the backend's own error `E` (an
/// [`AccountCode`](arkiv_reth_entitystore::AccountCode) or
/// [`IndexStorage`](crate::storage::IndexStorage) failure), plus the one error the
/// index logic itself can raise: a stored pair bitmap whose bytes won't deserialize.
#[derive(Debug)]
pub enum AuxError<E> {
    /// A backend read or write failed.
    Backend(E),
    /// A tier-1 pair account held bytes that aren't a valid bitmap — a corrupt or
    /// forked index.
    Bitmap(BitmapError),
}

impl<E> From<BitmapError> for AuxError<E> {
    fn from(error: BitmapError) -> Self {
        AuxError::Bitmap(error)
    }
}
