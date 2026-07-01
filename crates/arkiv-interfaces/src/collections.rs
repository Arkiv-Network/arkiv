//! Small collection types with stronger guarantees than the standard ones.

use alloc::vec::Vec;

/// A vector that always holds at least one element.
///
/// Used where an empty list would be meaningless — e.g. the values of an
/// [`In`](crate::query::Query::In) predicate. Non-emptiness is structural: the
/// first element is stored on its own, so an empty one cannot be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyVec<T> {
    /// The first element.
    pub first: T,
    /// Any elements after the first.
    pub rest: Vec<T>,
}

impl<T> NonEmptyVec<T> {
    /// A vector holding just `first`.
    pub fn one(first: T) -> Self {
        Self {
            first,
            rest: Vec::new(),
        }
    }

    /// Build from a `Vec`, or `None` if it is empty.
    pub fn from_vec(vec: Vec<T>) -> Option<Self> {
        let mut it = vec.into_iter();
        let first = it.next()?;
        Some(Self {
            first,
            rest: it.collect(),
        })
    }

    /// The number of elements (always at least one).
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        1 + self.rest.len()
    }

    /// Iterate over every element, `first` before `rest`.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        core::iter::once(&self.first).chain(self.rest.iter())
    }
}
