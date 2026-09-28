//! How records and changesets are laid out as bytes, and how the digest is
//! accumulated.
//!
//! Valkey stores opaque byte strings, so every structured value crossing the
//! wire is encoded here and nowhere else. The encodings are **canonical** —
//! one byte form per value — because the digest is taken over them, and two
//! nodes must agree.

use std::collections::BTreeMap;

use arkiv_interfaces::store::{Cell, CellName, Record, RecordKey, RecordVersion};

/// Marks an absent record: a branch tombstone, or the empty side of a change.
const ABSENT: u8 = 0x00;
/// Marks a present record, followed by its version and cells.
const PRESENT: u8 = 0x01;

/// A record as held in a branch diff or a changeset entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Stored {
    pub(crate) version: RecordVersion,
    pub(crate) cells: BTreeMap<CellName, Cell>,
}

impl Stored {
    /// Present this record to a caller, applying a projection if one was
    /// asked for.
    pub(crate) fn to_record(&self, key: RecordKey, projection: Option<&[CellName]>) -> Record {
        Record {
            key,
            version: self.version,
            cells: self
                .cells
                .iter()
                .filter(|(name, _)| projection.is_none_or(|wanted| wanted.contains(name)))
                .map(|(name, cell)| (name.clone(), cell.clone()))
                .collect(),
        }
    }
}

/// Encode a record, or its absence.
///
/// Cells are length-framed and emitted in name order — the map is ordered, so
/// the same content always produces the same bytes.
pub(crate) fn encode_record(record: Option<&Stored>) -> Vec<u8> {
    let Some(record) = record else {
        return vec![ABSENT];
    };

    let mut out = vec![PRESENT];
    out.extend_from_slice(&record.version.0.to_be_bytes());
    out.extend_from_slice(&(record.cells.len() as u32).to_be_bytes());
    for (name, cell) in &record.cells {
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(cell.value.len() as u32).to_be_bytes());
        out.push(cell.tag());
        out.extend_from_slice(&cell.value);
    }
    out
}

/// The inverse of [`encode_record`]. `None` for a well-formed absent marker,
/// and an error for anything that does not decode.
pub(crate) fn decode_record(bytes: &[u8]) -> Result<Option<Stored>, &'static str> {
    let mut cursor = Cursor::new(bytes);
    match cursor.byte()? {
        ABSENT => Ok(None),
        PRESENT => {
            let version = RecordVersion(u64::from_be_bytes(cursor.array::<8>()?));
            let count = u32::from_be_bytes(cursor.array::<4>()?);
            let mut cells = BTreeMap::new();
            for _ in 0..count {
                let name_len = u16::from_be_bytes(cursor.array::<2>()?) as usize;
                let name = core::str::from_utf8(cursor.slice(name_len)?)
                    .map_err(|_| "cell name is not utf-8")?
                    .to_owned();
                let value_len = u32::from_be_bytes(cursor.array::<4>()?) as usize;
                let (kind, type_id) =
                    Cell::split_tag(cursor.byte()?).ok_or("cell tag names no type")?;
                let value = cursor.slice(value_len)?.to_vec();
                cells.insert(
                    name,
                    Cell {
                        kind,
                        type_id,
                        value,
                    },
                );
            }
            Ok(Some(Stored { version, cells }))
        }
        _ => Err("record marker is neither present nor absent"),
    }
}

/// Encode one changeset entry: the key, then the record before and after.
pub(crate) fn encode_change(
    key: RecordKey,
    before: Option<&Stored>,
    after: Option<&Stored>,
) -> Vec<u8> {
    let mut out = Vec::from(key.0);
    out.extend_from_slice(&encode_record(before));
    out.extend_from_slice(&encode_record(after));
    out
}

/// The inverse of [`encode_change`].
pub(crate) fn decode_change(
    bytes: &[u8],
) -> Result<(RecordKey, Option<Stored>, Option<Stored>), &'static str> {
    let mut cursor = Cursor::new(bytes);
    let key = RecordKey(cursor.array::<32>()?);
    // Both records are decoded from the same cursor, so the second starts
    // wherever the first ended.
    let before = decode_record_at(&mut cursor)?;
    let after = decode_record_at(&mut cursor)?;
    Ok((key, before, after))
}

/// A record's contribution to the state digest.
///
/// **Not a commitment.** FNV-1a over the key and the canonical cell bytes,
/// four seeds wide. The record's version is excluded, as
/// [`branch_digest`](arkiv_interfaces::store::Store::branch_digest) requires: a
/// write that leaves content unchanged must not move the digest.
pub(crate) fn record_digest(key: RecordKey, record: &Stored) -> [u8; 32] {
    let mut encoded = Vec::from(key.0);
    // Deliberately not `encode_record`: that frames the version, which the
    // digest must not see.
    for (name, cell) in &record.cells {
        encoded.extend_from_slice(&(name.len() as u16).to_be_bytes());
        encoded.extend_from_slice(name.as_bytes());
        encoded.push(cell.tag());
        encoded.extend_from_slice(&(cell.value.len() as u32).to_be_bytes());
        encoded.extend_from_slice(&cell.value);
    }

    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x100_0000_01b3;
    const SEED_STRIDE: u64 = 0x9E37_79B9_7F4A_7C15;

    let mut digest = [0u8; 32];
    for (lane, chunk) in digest.chunks_mut(8).enumerate() {
        let mut accumulator = FNV_OFFSET_BASIS ^ (lane as u64).wrapping_mul(SEED_STRIDE);
        for byte in &encoded {
            accumulator ^= *byte as u64;
            accumulator = accumulator.wrapping_mul(FNV_PRIME);
        }
        chunk.copy_from_slice(&accumulator.to_be_bytes());
    }
    digest
}

/// Fold one record's digest into (or out of) an accumulator.
///
/// XOR is its own inverse, which is what makes the state digest **incremental**
/// rather than a rescan: replacing a record is "fold the old one out, fold the
/// new one in", and the empty state is all zeros. It is also order-independent,
/// so two nodes reaching the same content agree regardless of write order.
///
/// The price is that XOR is not collision-resistant. That is acceptable for a
/// divergence detector and unacceptable for a commitment — see the crate docs.
pub(crate) fn fold_digest(accumulator: &mut [u8; 32], record: [u8; 32]) {
    for (slot, byte) in accumulator.iter_mut().zip(record) {
        *slot ^= byte;
    }
}

// ---------------------------------------------------------------------------
// cursor
// ---------------------------------------------------------------------------

/// Decode one record from a cursor already positioned at its marker.
fn decode_record_at(cursor: &mut Cursor<'_>) -> Result<Option<Stored>, &'static str> {
    let start = cursor.position;
    // Peek the shape, then hand the whole remainder to the standalone decoder
    // and advance by however much it consumed.
    let consumed = record_len(&cursor.bytes[start..])?;
    let record = decode_record(&cursor.bytes[start..start + consumed])?;
    cursor.position += consumed;
    Ok(record)
}

/// How many bytes the record at the front of `bytes` occupies.
fn record_len(bytes: &[u8]) -> Result<usize, &'static str> {
    let mut cursor = Cursor::new(bytes);
    match cursor.byte()? {
        ABSENT => Ok(1),
        PRESENT => {
            cursor.array::<8>()?;
            let count = u32::from_be_bytes(cursor.array::<4>()?);
            for _ in 0..count {
                let name_len = u16::from_be_bytes(cursor.array::<2>()?) as usize;
                cursor.slice(name_len)?;
                let value_len = u32::from_be_bytes(cursor.array::<4>()?) as usize;
                cursor.byte()?;
                cursor.slice(value_len)?;
            }
            Ok(cursor.position)
        }
        _ => Err("record marker is neither present nor absent"),
    }
}

/// A bounds-checked reader over an encoded value.
struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// The next byte.
    fn byte(&mut self) -> Result<u8, &'static str> {
        Ok(self.slice(1)?[0])
    }

    /// The next `N` bytes as a fixed-width array.
    fn array<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.slice(N)?);
        Ok(out)
    }

    /// The next `len` bytes, or an error if the input ends first.
    fn slice(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let end = self
            .position
            .checked_add(len)
            .ok_or("encoded value claims an impossible length")?;
        let slice = self
            .bytes
            .get(self.position..end)
            .ok_or("encoded value ends mid-field")?;
        self.position = end;
        Ok(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::store::TypeId;

    fn sample() -> Stored {
        let mut cells = BTreeMap::new();
        cells.insert("n".to_owned(), Cell::attribute(TypeId::U64, vec![0; 8]));
        cells.insert("blob".to_owned(), Cell::field(TypeId::BYTES, vec![1, 2, 3]));
        Stored {
            version: RecordVersion(7),
            cells,
        }
    }

    #[test]
    fn records_round_trip() {
        let record = sample();
        let encoded = encode_record(Some(&record));
        assert_eq!(decode_record(&encoded).unwrap(), Some(record));
        assert_eq!(decode_record(&encode_record(None)).unwrap(), None);
    }

    #[test]
    fn changes_round_trip_with_either_side_absent() {
        let key = RecordKey([9u8; 32]);
        let record = sample();
        for (before, after) in [
            (None, Some(&record)),
            (Some(&record), None),
            (Some(&record), Some(&record)),
        ] {
            let encoded = encode_change(key, before, after);
            let (decoded_key, decoded_before, decoded_after) = decode_change(&encoded).unwrap();
            assert_eq!(decoded_key, key);
            assert_eq!(decoded_before.as_ref(), before);
            assert_eq!(decoded_after.as_ref(), after);
        }
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        let encoded = encode_record(Some(&sample()));
        for cut in 1..encoded.len() {
            assert!(decode_record(&encoded[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn digest_ignores_the_record_version() {
        let key = RecordKey([1u8; 32]);
        let mut bumped = sample();
        bumped.version = RecordVersion(99);
        assert_eq!(record_digest(key, &sample()), record_digest(key, &bumped));
    }

    #[test]
    fn folding_a_record_twice_cancels_it() {
        let key = RecordKey([1u8; 32]);
        let digest = record_digest(key, &sample());
        let mut accumulator = [0u8; 32];
        fold_digest(&mut accumulator, digest);
        assert_ne!(accumulator, [0u8; 32]);
        fold_digest(&mut accumulator, digest);
        assert_eq!(accumulator, [0u8; 32], "the empty state is all zeros");
    }
}
