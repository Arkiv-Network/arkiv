//! The query **read path**: evaluate a [`Query`] to the bitmap of matching entity
//! ids.
//!
//! A straight tree walk, ported from `arkiv-db-engine`'s interpreter, over the
//! index's two halves:
//!
//! - **Equality** (`Eq`) reads one [`EqualityIndex`] bitmap — the query value's
//!   own type and index bytes name the same bucket the writer derived.
//! - **Negation** (`Not`) is `$all` minus the positive match — see
//!   [`complement`]. It is the only negation the language has: value-negation
//!   (`!=`) would need a per-`(attribute, type)` presence set the index does not
//!   maintain, so the parser rejects it rather than answering this wider set.
//! - **Range** (`Gt`/`Gte`/`Lt`/`Lte`) scans the [`RangeIndex`] for the matching
//!   *values*, then unions each value's equality bitmap ([`union_pair_bitmaps`]).
//!   This range → equality resolution is the whole reason values are recorded
//!   twice.
//! - **StartsWith** is a str-mode prefix scan, resolved the same way.
//! - **Boolean** `And`/`Or` intersect/union the sub-results, with `And`
//!   short-circuiting on an empty left side.
//!
//! Everything is keyed on the compact `u64` entity id; the caller
//! ([`store`](crate::indices::store)) maps the surviving ids back to keys.

use crate::entities::AccountCode;
use arkiv_interfaces::entity::AttributeType;
use arkiv_interfaces::query::{AnnotKey, AnnotVal, Query};

use crate::indices::annotation;
use crate::indices::bitmap::Bitmap;
use crate::indices::equality_index::EqualityIndex;
use crate::indices::error::AuxError;
use crate::indices::range::Bound;
use crate::indices::range_index::RangeIndex;
use crate::indices::storage::IndexStorage;

/// Evaluate `query` to the bitmap of entity ids that match it.
pub(crate) fn eval<B, E>(query: &Query, backend: &mut B) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
{
    let attr = |key: &AnnotKey| annotation::attr_bytes(key);
    match query {
        Query::All => EqualityIndex::new(backend).all_entities(),

        Query::Eq { key, value } => eq_bitmap(backend, &attr(key), value),

        Query::Gt { key, value } => range_bitmap(backend, &attr(key), value, Bound::Gt),
        Query::Gte { key, value } => range_bitmap(backend, &attr(key), value, Bound::Gte),
        Query::Lt { key, value } => range_bitmap(backend, &attr(key), value, Bound::Lt),
        Query::Lte { key, value } => range_bitmap(backend, &attr(key), value, Bound::Lte),

        Query::StartsWith { key, value } => prefix_bitmap(backend, &attr(key), value),

        Query::And(left, right) => {
            let mut hits = eval(left, backend)?;
            if hits.is_empty() {
                return Ok(hits); // short-circuit: nothing can survive the intersection
            }
            let other = eval(right, backend)?;
            hits.intersect_with(&other);
            Ok(hits)
        }
        Query::Or(left, right) => {
            let mut hits = eval(left, backend)?;
            let other = eval(right, backend)?;
            hits.union_with(&other);
            Ok(hits)
        }
        Query::Not(inner) => complement(backend, |b| eval(inner, b)),
    }
}

/// `$all` minus whatever `positive` matches — how every negation is evaluated.
fn complement<B, E, F>(backend: &mut B, positive: F) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
    F: FnOnce(&mut B) -> Result<Bitmap, AuxError<E>>,
{
    let mut all = EqualityIndex::new(&mut *backend).all_entities()?;
    let matching = positive(backend)?;
    all.subtract(&matching);
    Ok(all)
}

/// The equality bitmap for a single `(attr, value)` — the query value's own type
/// and bytes name the same bucket the writer derived.
pub(crate) fn eq_bitmap<B, E>(
    backend: &mut B,
    attr: &[u8],
    value: &AnnotVal,
) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E>,
{
    EqualityIndex::new(backend).get(attr, value.attr_type(), &value.index_bytes())
}

/// Resolve a range predicate: scan the [`RangeIndex`] for the matching values,
/// then union their equality bitmaps.
pub(crate) fn range_bitmap<B, E>(
    backend: &mut B,
    attr: &[u8],
    value: &AnnotVal,
    bound: Bound,
) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
{
    let ty = value.attr_type();
    let capabilities = annotation::capabilities_for(attr, ty);
    let values =
        RangeIndex::new(&mut *backend).scan(attr, ty, &value.index_bytes(), bound, capabilities)?;
    union_pair_bitmaps(backend, attr, ty, values)
}

/// Resolve a `STARTSWITH` predicate: a str-mode prefix scan, then union the
/// equality bitmaps.
pub(crate) fn prefix_bitmap<B, E>(
    backend: &mut B,
    attr: &[u8],
    value: &AnnotVal,
) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
{
    let ty = value.attr_type();
    let values =
        RangeIndex::new(&mut *backend).get_prefix_matches(attr, ty, &value.index_bytes())?;
    union_pair_bitmaps(backend, attr, ty, values)
}

/// Union the equality bitmaps of `attr` for each value a range scan returned.
fn union_pair_bitmaps<B, E>(
    backend: &mut B,
    attr: &[u8],
    ty: AttributeType,
    values: Vec<Vec<u8>>,
) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E>,
{
    let mut equality = EqualityIndex::new(backend);
    let mut hits = Bitmap::new();
    for value in &values {
        hits.union_with(&equality.get(attr, ty, value)?);
    }
    Ok(hits)
}
