//! Shared storage-slot encoding: a `u64` right-aligned in a 32-byte word.
//!
//! Both the [`btree`](crate::btree) (node ids, child pointers, slot numbers) and
//! the string [`cascade`](crate::cascade)'s enumeration lists pack `u64`s into
//! storage words this way, so the encoders live here rather than in either.

use alloy_primitives::B256;
use arkiv_interfaces::constants::WORD_LEN;

const _: () = assert!(size_of::<B256>() == WORD_LEN, "a storage word is a B256");

/// Encode a `u64` right-aligned in a storage word: the low `size_of::<u64>()` bytes
/// hold the value big-endian, the rest is zero.
#[inline]
pub(crate) fn u64_to_storage(value: u64) -> B256 {
    let mut buf = [0u8; WORD_LEN];
    buf[WORD_LEN - size_of::<u64>()..].copy_from_slice(&value.to_be_bytes());
    B256::from(buf)
}

/// Read a `u64` back from the low bytes of a right-aligned storage word — the exact
/// inverse of [`u64_to_storage`].
#[inline]
pub(crate) fn storage_to_u64(word: B256) -> u64 {
    u64::from_be_bytes(word.0[WORD_LEN - size_of::<u64>()..].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_right_aligned() {
        let word = u64_to_storage(0x0102);
        assert_eq!(storage_to_u64(word), 0x0102);
        // High bytes zero; the value sits big-endian in the low size_of::<u64>() bytes.
        assert_eq!(
            &word.0[..WORD_LEN - size_of::<u64>()],
            &[0u8; WORD_LEN - size_of::<u64>()]
        );
        assert_eq!(word.0[WORD_LEN - 2], 0x01);
        assert_eq!(word.0[WORD_LEN - 1], 0x02);
    }
}
