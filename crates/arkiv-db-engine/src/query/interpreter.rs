//! Tree-walking evaluator for the parsed query AST + the top-level
//! [`execute`] entry point that callers (RPC, tests) should use.
//!
//! Range/glob predicates dispatch to either the single-level **int
//! index** (values ≤ 32 bytes) or the four-level **string index**
//! (values ≤ 128 bytes) via [`AnnotMode`] stored in the AST.

use eyre::Result;

use super::parser::{AnnotKey, AnnotVal, Query, parse};
use crate::{
    AnnotMode, Bitmap, EntityRlp, StateAdapter, all_entities,
    annot_val_to_slot, btree_header_address, read_pair_bitmap, resolve_id,
    slot_to_val_len, str_level_address,
};
use alloy_primitives::B256;

impl Query {
    /// Evaluate the AST against `state`, returning the bitmap of
    /// matching entity IDs.
    pub fn evaluate<S: StateAdapter>(&self, state: &mut S) -> Result<Bitmap> {
        eval(self, state)
    }
}

fn eval<S: StateAdapter>(query: &Query, state: &mut S) -> Result<Bitmap> {
    match query {
        Query::All => all_entities(state),

        Query::Eq { key, value } => read_eq(state, key, value),
        Query::Neq { key, value } => {
            let mut all = all_entities(state)?;
            let hit = read_eq(state, key, value)?;
            all.subtract(&hit);
            Ok(all)
        }

        Query::In { key, values } => read_in(state, key, values),
        Query::NotIn { key, values } => {
            let mut all = all_entities(state)?;
            let hit = read_in(state, key, values)?;
            all.subtract(&hit);
            Ok(all)
        }

        Query::And(left, right) => {
            let mut l = eval(left, state)?;
            if l.is_empty() {
                return Ok(l);
            }
            let r = eval(right, state)?;
            l.intersect_with(&r);
            Ok(l)
        }
        Query::Or(left, right) => {
            let mut l = eval(left, state)?;
            let r = eval(right, state)?;
            l.union_with(&r);
            Ok(l)
        }
        Query::Not(inner) => {
            let mut all = all_entities(state)?;
            let hit = eval(inner, state)?;
            all.subtract(&hit);
            Ok(all)
        }

        Query::Gt { key, value, mode } => {
            collect_range_bitmaps(state, key.pair_key_bytes(), &value.0, *mode, Bound::Gt)
        }
        Query::Gte { key, value, mode } => {
            collect_range_bitmaps(state, key.pair_key_bytes(), &value.0, *mode, Bound::Gte)
        }
        Query::Lt { key, value, mode } => {
            collect_range_bitmaps(state, key.pair_key_bytes(), &value.0, *mode, Bound::Lt)
        }
        Query::Lte { key, value, mode } => {
            collect_range_bitmaps(state, key.pair_key_bytes(), &value.0, *mode, Bound::Lte)
        }
        Query::Glob { key, value } => {
            collect_glob_bitmaps(state, key.pair_key_bytes(), &value.0)
        }
        Query::NotGlob { key, value } => {
            let mut all = all_entities(state)?;
            let hit = collect_glob_bitmaps(state, key.pair_key_bytes(), &value.0)?;
            all.subtract(&hit);
            Ok(all)
        }
    }
}

// ── Range bound kind ─────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Bound {
    Gt,
    Gte,
    Lt,
    Lte,
}

// ── Main dispatch ─────────────────────────────────────────────────────

fn collect_range_bitmaps<S: StateAdapter>(
    state: &mut S,
    attr_key: &[u8],
    bound: &[u8],
    mode: AnnotMode,
    kind: Bound,
) -> Result<Bitmap> {
    let vals = match mode {
        AnnotMode::Int => int_range_values(state, attr_key, bound, kind)?,
        AnnotMode::Str => str_range_values(state, attr_key, bound, kind)?,
    };
    let mut result = Bitmap::new();
    for v in &vals {
        result.union_with(&read_pair_bitmap(state, attr_key, v)?);
    }
    Ok(result)
}

fn collect_glob_bitmaps<S: StateAdapter>(
    state: &mut S,
    attr_key: &[u8],
    prefix: &[u8],
) -> Result<Bitmap> {
    // Glob is always Str mode (parser enforces this).
    let vals = str_glob_values(state, attr_key, prefix)?;
    let mut result = Bitmap::new();
    for v in &vals {
        result.union_with(&read_pair_bitmap(state, attr_key, v)?);
    }
    Ok(result)
}

// ── Int index (single-level storage slots, values ≤ 32 bytes) ────────

fn int_range_values<S: StateAdapter>(
    state: &mut S,
    attr_key: &[u8],
    bound: &[u8],
    kind: Bound,
) -> Result<Vec<Vec<u8>>> {
    let addr = btree_header_address(attr_key);
    let bound_slot = annot_val_to_slot(bound);
    let mut result = Vec::new();

    match kind {
        Bound::Gt | Bound::Gte => {
            let inclusive = matches!(kind, Bound::Gte);
            for (slot_key, slot_val) in state.iter_storage_asc(&addr, bound_slot)? {
                let Some(len) = slot_to_val_len(slot_val) else { continue };
                if !inclusive && slot_key == bound_slot {
                    continue;
                }
                result.push(slot_key.0[..len].to_vec());
            }
        }
        Bound::Lt | Bound::Lte => {
            let inclusive = matches!(kind, Bound::Lte);
            for (slot_key, slot_val) in state.iter_storage_asc(&addr, B256::ZERO)? {
                if slot_key > bound_slot {
                    break;
                }
                if !inclusive && slot_key == bound_slot {
                    break;
                }
                let Some(len) = slot_to_val_len(slot_val) else { continue };
                result.push(slot_key.0[..len].to_vec());
            }
        }
    }
    Ok(result)
}

// ── String index (four-level DFS, values ≤ 128 bytes) ────────────────

fn str_range_values<S: StateAdapter>(
    state: &mut S,
    attr_key: &[u8],
    bound: &[u8],
    kind: Bound,
) -> Result<Vec<Vec<u8>>> {
    let mut all = Vec::new();
    str_collect_all(state, attr_key, &[], &mut all)?;

    let result = all
        .into_iter()
        .filter(|v| match kind {
            Bound::Gt => v.as_slice() > bound,
            Bound::Gte => v.as_slice() >= bound,
            Bound::Lt => v.as_slice() < bound,
            Bound::Lte => v.as_slice() <= bound,
        })
        .collect();
    Ok(result)
}

fn str_glob_values<S: StateAdapter>(
    state: &mut S,
    attr_key: &[u8],
    prefix: &[u8],
) -> Result<Vec<Vec<u8>>> {
    let mut all = Vec::new();
    str_collect_all(state, attr_key, &[], &mut all)?;

    let result = all
        .into_iter()
        .filter(|v| v.starts_with(prefix))
        .collect();
    Ok(result)
}

/// DFS over the string index, collecting all stored values in ascending
/// lex order (because `iter_storage_asc` yields slots in key order).
fn str_collect_all<S: StateAdapter>(
    state: &mut S,
    attr_key: &[u8],
    prefix: &[u8],
    result: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let level = prefix.len() / 32;
    let addr = str_level_address(attr_key, prefix);
    for (chunk, slot_val) in state.iter_storage_asc(&addr, B256::ZERO)? {
        let Some(total_len) = slot_to_val_len(slot_val) else { continue };
        if total_len <= (level + 1) * 32 {
            // Leaf: reconstruct full value from prefix + tail bytes of chunk.
            let tail_len = total_len.saturating_sub(level * 32);
            let mut val = prefix.to_vec();
            val.extend_from_slice(&chunk.0[..tail_len]);
            result.push(val);
        } else {
            // Internal node: recurse into the next level.
            let mut next_prefix = prefix.to_vec();
            next_prefix.extend_from_slice(chunk.as_slice());
            str_collect_all(state, attr_key, &next_prefix, result)?;
        }
    }
    Ok(())
}

// ── Point-read helpers ────────────────────────────────────────────────

fn read_eq<S: StateAdapter>(state: &mut S, key: &AnnotKey, value: &AnnotVal) -> Result<Bitmap> {
    read_pair_bitmap(state, key.pair_key_bytes(), &value.0)
}

fn read_in<S: StateAdapter>(
    state: &mut S,
    key: &AnnotKey,
    values: &[AnnotVal],
) -> Result<Bitmap> {
    let mut acc = Bitmap::new();
    for v in values {
        let bm = read_pair_bitmap(state, key.pair_key_bytes(), &v.0)?;
        acc.union_with(&bm);
    }
    Ok(acc)
}

// ── Top-level entry point ─────────────────────────────────────────────

/// Parameters for paginated query execution.
#[derive(Debug, Clone, Copy)]
pub struct PageParams {
    /// Maximum number of entities to return in this page. Must be > 0.
    pub page_size: u64,
    /// If `Some(c)`, only return IDs strictly less than `c`.
    pub cursor: Option<u64>,
}

/// One page of query results, ordered descending by entity ID.
#[derive(Debug, Clone)]
pub struct Page {
    pub entries: Vec<EntityRlp>,
    pub next_cursor: Option<u64>,
}

/// Parse → evaluate → paginate → resolve, in one call.
pub fn execute<S: StateAdapter>(state: &mut S, query: &str, params: PageParams) -> Result<Page> {
    eyre::ensure!(params.page_size > 0, "page_size must be > 0");

    let parsed = parse(query)?;
    let bitmap = parsed.evaluate(state)?;

    let mut ids: Vec<u64> = bitmap.iter().collect();
    ids.sort_unstable();
    if let Some(c) = params.cursor {
        while ids.last().is_some_and(|id| *id >= c) {
            ids.pop();
        }
    }

    let page_size = params.page_size as usize;
    let mut entries = Vec::with_capacity(page_size.min(ids.len()));
    let mut last_returned_id: Option<u64> = None;
    let mut has_more = false;

    for &id in ids.iter().rev() {
        if entries.len() >= page_size {
            has_more = true;
            break;
        }
        if let Some(entity) = resolve_id(state, id)? {
            entries.push(entity);
            last_returned_id = Some(id);
        }
    }

    let next_cursor = if has_more { last_returned_id } else { None };
    Ok(Page {
        entries,
        next_cursor,
    })
}
