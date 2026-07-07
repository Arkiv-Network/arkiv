//! Arkiv DA (data-availability) format — the frozen wire contract the committer
//! writes to the base-chain inbox, and the offline decoder / deriver / harness
//! read back (contract C2 in `docs/september-action-plan.md`).
//!
//! Payload layout: `DA_VERSION (1 byte) || zstd(rlp(block))`.
//!
//! Compression is built in: the RLP block is zstd-compressed before framing, so
//! a `BlockPosted` log carries the smallest faithful copy of the block. Decoding
//! reverses it — strip the version byte, zstd-decompress, RLP-decode.
//!
//! DA encoding **v1 is provisional**: the Committer owner (Piotr) freezes the
//! final field list. Until then this is the shared definition everyone builds on.

use alloy_primitives::{B256, keccak256};

/// DA format version — the first byte of every payload.
pub const DA_VERSION: u8 = 1;

/// zstd compression level used for the payload (zstd's own default).
pub const ZSTD_LEVEL: i32 = 3;

/// Frame and compress already-RLP-encoded block bytes into a DA payload.
pub fn encode_bytes(rlp_block: &[u8]) -> Vec<u8> {
    let compressed =
        zstd::encode_all(rlp_block, ZSTD_LEVEL).expect("zstd of in-memory bytes is infallible");
    let mut out = Vec::with_capacity(1 + compressed.len());
    out.push(DA_VERSION);
    out.extend_from_slice(&compressed);
    out
}

/// Reverse [`encode_bytes`]: validate the version, decompress, return the RLP block bytes.
pub fn decode_bytes(payload: &[u8]) -> Result<Vec<u8>, DaError> {
    let (&version, body) = payload.split_first().ok_or(DaError::Empty)?;
    if version != DA_VERSION {
        return Err(DaError::UnsupportedVersion(version));
    }
    zstd::decode_all(body).map_err(DaError::Decompress)
}

/// RLP-encode, then frame + compress, a block (or any RLP-encodable value).
pub fn encode_block<B: alloy_rlp::Encodable>(block: &B) -> Vec<u8> {
    encode_bytes(&alloy_rlp::encode(block))
}

/// Decode a DA payload back into a block (or any RLP-decodable value).
pub fn decode_block<B: alloy_rlp::Decodable>(payload: &[u8]) -> Result<B, DaError> {
    let rlp = decode_bytes(payload)?;
    B::decode(&mut rlp.as_slice()).map_err(DaError::Rlp)
}

/// Length of an Ethereum ABI function selector, in bytes: the leading 4 bytes of
/// `keccak256(signature)`.
const SELECTOR_LEN: usize = 4;

/// Selector for the inbox call `postBlock(bytes)` (contract C2) — the first
/// [`SELECTOR_LEN`] bytes of the signature hash.
pub fn post_block_selector() -> [u8; SELECTOR_LEN] {
    keccak256("postBlock(bytes)")[..SELECTOR_LEN]
        .try_into()
        .unwrap()
}

/// topic0 of the inbox event `BlockPosted(uint256,bytes32,bytes)` (contract C2).
pub fn block_posted_topic0() -> B256 {
    keccak256("BlockPosted(uint256,bytes32,bytes)")
}

/// Errors from decoding a DA payload.
#[derive(Debug)]
pub enum DaError {
    /// Payload was empty (no version byte).
    Empty,
    /// Version byte did not match [`DA_VERSION`].
    UnsupportedVersion(u8),
    /// zstd decompression failed (corrupt payload).
    Decompress(std::io::Error),
    /// RLP decoding of the decompressed bytes failed.
    Rlp(alloy_rlp::Error),
}

impl core::fmt::Display for DaError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty DA payload"),
            Self::UnsupportedVersion(v) => {
                write!(f, "unsupported DA version {v} (expected {DA_VERSION})")
            }
            Self::Decompress(e) => write!(f, "zstd decompress failed: {e}"),
            Self::Rlp(e) => write!(f, "RLP decode failed: {e}"),
        }
    }
}

impl std::error::Error for DaError {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;

    #[test]
    fn round_trips_a_block_header() {
        let header = Header::default();
        let payload = encode_block(&header);
        assert_eq!(payload[0], DA_VERSION);
        let decoded: Header = decode_block(&payload).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn rejects_bad_version() {
        let mut payload = encode_bytes(b"anything");
        payload[0] = 0xff;
        assert!(matches!(
            decode_block::<Header>(&payload),
            Err(DaError::UnsupportedVersion(0xff))
        ));
    }

    #[test]
    fn compression_shrinks_repetitive_data() {
        let rlp = vec![0u8; 10_000];
        let payload = encode_bytes(&rlp);
        assert!(payload.len() < rlp.len());
        assert_eq!(decode_bytes(&payload).unwrap(), rlp);
    }
}
