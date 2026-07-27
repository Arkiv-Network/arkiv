//! The query **read path**: evaluate a [`Query`] to the bitmap of matching entity
//! ids.
//!
//! A straight tree walk, ported from `arkiv-db-engine`'s interpreter:
//!
//! - **Equality** (`Eq`/`In`) reads pair bitmaps and unions them.
//! - **Negation** (`Neq`/`NotIn`/`Not`/`NotGlob`) is `$all` minus the positive
//!   match — see [`complement`].
//! - **Range** (`Gt`/`Gte`/`Lt`/`Lte`) scans the tier-2 index for the matching
//!   *values*, then unions each value's tier-1 pair bitmap ([`union_pair_bitmaps`]).
//!   This tier-2 → tier-1 resolution is the whole reason values are recorded twice.
//! - **Glob** is a str-mode prefix scan, resolved the same way.
//! - **Boolean** `And`/`Or` intersect/union the sub-results, with `And`
//!   short-circuiting on an empty left side.
//!
//! Everything is keyed on the compact `u64` entity id; the caller
//! ([`store`](crate::store)) maps the surviving ids back to keys.

use arkiv_interfaces::collections::NonEmptyVec;
use arkiv_interfaces::entity::AttributeType;
use arkiv_interfaces::query::{AnnotKey, AnnotVal, Query};
use arkiv_reth_entitystore::AccountCode;

use crate::address::{all_entities_bucket, pair_address};
use crate::annotation::{self, Mode};
use crate::bitmap::Bitmap;
use crate::error::AuxError;
use crate::index::read_pair_bitmap;
use crate::range::Bound;
use crate::storage::IndexStorage;
use crate::{cascade, range};

/// Evaluate `query` to the bitmap of entity ids that match it.
pub(crate) fn eval<B, E>(query: &Query, backend: &mut B) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
{
    match query {
        Query::All => all_entities(backend),

        Query::Eq { key, value } => read_eq(backend, key, value),
        Query::Neq { key, value } => complement(backend, |b| read_eq(b, key, value)),

        Query::In { key, values } => read_in(backend, key, values),
        Query::NotIn { key, values } => complement(backend, |b| read_in(b, key, values)),

        Query::Gt { key, value } => range_bitmaps(backend, key, value, Bound::Gt),
        Query::Gte { key, value } => range_bitmaps(backend, key, value, Bound::Gte),
        Query::Lt { key, value } => range_bitmaps(backend, key, value, Bound::Lt),
        Query::Lte { key, value } => range_bitmaps(backend, key, value, Bound::Lte),

        Query::Glob { key, value } => glob_bitmaps(backend, key, value),
        Query::NotGlob { key, value } => complement(backend, |b| glob_bitmaps(b, key, value)),

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

/// Every live entity: the `($all, "")` bucket.
fn all_entities<B, E>(backend: &mut B) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E>,
{
    read_pair_bitmap(backend, all_entities_bucket())
}

/// `$all` minus whatever `positive` matches — how every negation is evaluated.
fn complement<B, E, F>(backend: &mut B, positive: F) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
    F: FnOnce(&mut B) -> Result<Bitmap, AuxError<E>>,
{
    let mut all = all_entities(backend)?;
    let matching = positive(backend)?;
    all.subtract(&matching);
    Ok(all)
}

/// The pair bitmap for a single `(key, value)` equality.
fn read_eq<B, E>(backend: &mut B, key: &AnnotKey, value: &AnnotVal) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E>,
{
    let attr = annotation::attr_bytes(key);
    read_pair_bitmap(backend, value_pair_address(&attr, value))
}

/// The bucket a query value's own type and bytes name — the same address the
/// writer derived for an equal attribute value.
fn value_pair_address(attr: &[u8], value: &AnnotVal) -> alloy_primitives::Address {
    pair_address(attr, value.attr_type(), &value.index_bytes())
}

/// The union of pair bitmaps for `key` equal to any of `values`.
fn read_in<B, E>(
    backend: &mut B,
    key: &AnnotKey,
    values: &NonEmptyVec<AnnotVal>,
) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E>,
{
    let attr = annotation::attr_bytes(key);
    let mut hits = Bitmap::new();
    for value in values.iter() {
        let bucket = read_pair_bitmap(backend, value_pair_address(&attr, value))?;
        hits.union_with(&bucket);
    }
    Ok(hits)
}

/// Resolve a range predicate: scan the tier-2 index for the matching values, then
/// union their tier-1 pair bitmaps.
fn range_bitmaps<B, E>(
    backend: &mut B,
    key: &AnnotKey,
    value: &AnnotVal,
    bound: Bound,
) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
{
    let attr = annotation::attr_bytes(key);
    let ty = value.attr_type();
    let mode = annotation::mode_for(&attr, ty);
    let bound_bytes = value.index_bytes();
    let values = match mode {
        Mode::Range => {
            range::scan(backend, &attr, ty, &bound_bytes, bound).map_err(AuxError::Backend)?
        }
        Mode::Prefix => {
            cascade::scan(backend, &attr, ty, &bound_bytes, bound).map_err(AuxError::Backend)?
        }
        // A range over an unordered attribute has no tier-2 index to scan; the parser
        // rejects such queries, so if one reaches here it simply matches nothing.
        Mode::Equality | Mode::None => Vec::new(),
    };
    union_pair_bitmaps(backend, &attr, ty, values)
}

/// Resolve a glob predicate: a str-mode prefix scan, then union the pair bitmaps.
fn glob_bitmaps<B, E>(
    backend: &mut B,
    key: &AnnotKey,
    value: &AnnotVal,
) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E> + IndexStorage<Error = E>,
{
    let attr = annotation::attr_bytes(key);
    let ty = value.attr_type();
    let prefix = value.index_bytes();
    let values = cascade::glob(backend, &attr, ty, &prefix).map_err(AuxError::Backend)?;
    union_pair_bitmaps(backend, &attr, ty, values)
}

/// Union the tier-1 pair bitmaps of `attr` for each value a tier-2 scan returned.
fn union_pair_bitmaps<B, E>(
    backend: &mut B,
    attr: &[u8],
    ty: AttributeType,
    values: Vec<Vec<u8>>,
) -> Result<Bitmap, AuxError<E>>
where
    B: AccountCode<Error = E>,
{
    let mut hits = Bitmap::new();
    for value in &values {
        let bucket = read_pair_bitmap(backend, pair_address(attr, ty, value))?;
        hits.union_with(&bucket);
    }
    Ok(hits)
}
