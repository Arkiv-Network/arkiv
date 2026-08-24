//! The equality index's value type: a roaring64 bitmap of entity ids.
//!
//! Every tier-1 *pair account* holds one of these — the set of entity ids that
//! carry a given `(attribute, value)` pair. Query evaluation combines them with
//! set operations: `And` intersects, `Or` unions, negation subtracts from the
//! `$all` bitmap.
//!
//! **Determinism is load-bearing.** [`Bitmap::to_bytes`] must produce identical
//! bytes for any two bitmaps holding the same set of ids: a pair account is
//! content-addressed in the trie (`codeHash = keccak256(bitmap_bytes)`), so two
//! nodes that agree on the set must agree on the bytes, hence on the state root.
//! [`roaring`]'s portable serialization gives us exactly that; the crate is pinned
//! so it can't shift underneath us.

use roaring::RoaringTreemap;

/// A roaring64 bitmap of entity ids.
///
/// Entity ids are `u64` (matching
/// [`AuxiliaryEntityDelta`](crate::indices::delta::AuxiliaryEntityDelta)),
/// so this wraps the 64-bit [`RoaringTreemap`] rather than the 32-bit
/// `RoaringBitmap`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bitmap(RoaringTreemap);

impl Bitmap {
    /// An empty bitmap.
    pub fn new() -> Self {
        Self(RoaringTreemap::new())
    }

    /// Deserialize from the portable roaring layout produced by [`to_bytes`].
    ///
    /// [`to_bytes`]: Bitmap::to_bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BitmapError> {
        RoaringTreemap::deserialize_from(bytes)
            .map(Self)
            .map_err(BitmapError::Deserialize)
    }

    /// Serialize to the portable roaring layout. The same set of ids always
    /// produces the same bytes — see the module docs on why that matters.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.0.serialized_size());
        self.0
            .serialize_into(&mut buf)
            .expect("writing to a Vec is infallible");
        buf
    }

    /// Add `id` to the set. Returns whether it was newly added.
    pub fn insert(&mut self, id: u64) -> bool {
        self.0.insert(id)
    }

    /// Remove `id` from the set. Returns whether it was present.
    pub fn remove(&mut self, id: u64) -> bool {
        self.0.remove(id)
    }

    /// Whether `id` is in the set.
    pub fn contains(&self, id: u64) -> bool {
        self.0.contains(id)
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The number of ids in the set.
    pub fn len(&self) -> u64 {
        self.0.len()
    }

    /// The ids in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter()
    }

    /// In-place set union: `self ∪= other`.
    pub fn union_with(&mut self, other: &Bitmap) {
        self.0 |= &other.0;
    }

    /// In-place set intersection: `self ∩= other`.
    pub fn intersect_with(&mut self, other: &Bitmap) {
        self.0 &= &other.0;
    }

    /// In-place set difference: `self \= other`.
    pub fn subtract(&mut self, other: &Bitmap) {
        self.0 -= &other.0;
    }
}

impl FromIterator<u64> for Bitmap {
    fn from_iter<I: IntoIterator<Item = u64>>(iter: I) -> Self {
        Self(RoaringTreemap::from_iter(iter))
    }
}

/// Why [`Bitmap::from_bytes`] failed.
#[derive(Debug)]
pub enum BitmapError {
    /// The bytes weren't a valid portable roaring bitmap.
    Deserialize(std::io::Error),
}

impl core::fmt::Display for BitmapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BitmapError::Deserialize(e) => write!(f, "invalid roaring bitmap bytes: {e}"),
        }
    }
}

impl std::error::Error for BitmapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BitmapError::Deserialize(e) => Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bitmap_is_empty() {
        let b = Bitmap::new();
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
        assert!(!b.contains(0));
    }

    #[test]
    fn insert_remove_contains() {
        let mut b = Bitmap::new();
        assert!(b.insert(42));
        assert!(!b.insert(42)); // already present
        assert!(b.contains(42));
        assert_eq!(b.len(), 1);
        assert!(b.remove(42));
        assert!(!b.remove(42)); // already absent
        assert!(!b.contains(42));
        assert!(b.is_empty());
    }

    #[test]
    fn holds_full_u64_ids() {
        let mut b = Bitmap::new();
        b.insert(u64::MAX);
        b.insert(0);
        assert!(b.contains(u64::MAX));
        assert!(b.contains(0));
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![0, u64::MAX]);
    }

    #[test]
    fn iter_is_ascending() {
        let b = Bitmap::from_iter([9, 1, 5, 1, 7]);
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![1, 5, 7, 9]);
    }

    #[test]
    fn union_intersect_subtract() {
        let mut a = Bitmap::from_iter([1, 2, 3]);
        let b = Bitmap::from_iter([2, 3, 4]);

        let mut u = a.clone();
        u.union_with(&b);
        assert_eq!(u.iter().collect::<Vec<_>>(), vec![1, 2, 3, 4]);

        let mut i = a.clone();
        i.intersect_with(&b);
        assert_eq!(i.iter().collect::<Vec<_>>(), vec![2, 3]);

        a.subtract(&b);
        assert_eq!(a.iter().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn roundtrips_through_bytes() {
        let b = Bitmap::from_iter([0, 1, 100, 1_000_000, u64::MAX]);
        let bytes = b.to_bytes();
        assert_eq!(Bitmap::from_bytes(&bytes).unwrap(), b);
    }

    #[test]
    fn empty_roundtrips() {
        let b = Bitmap::new();
        assert_eq!(Bitmap::from_bytes(&b.to_bytes()).unwrap(), b);
    }

    /// The determinism guarantee the trie relies on: same set → same bytes,
    /// regardless of insertion order.
    #[test]
    fn serialization_is_deterministic() {
        let forward = Bitmap::from_iter([1, 2, 3, 4, 5]);
        let mut shuffled = Bitmap::new();
        for id in [5, 1, 4, 2, 3] {
            shuffled.insert(id);
        }
        assert_eq!(forward.to_bytes(), shuffled.to_bytes());
    }

    #[test]
    fn rejects_garbage_bytes() {
        assert!(matches!(
            Bitmap::from_bytes(&[0xFF, 0xFF, 0xFF]),
            Err(BitmapError::Deserialize(_))
        ));
    }
}
