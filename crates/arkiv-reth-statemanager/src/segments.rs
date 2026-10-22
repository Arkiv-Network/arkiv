//! What the append-only tier holds, and how a row is named.
//!
//! Three segments cover everything reth's database held that is not account
//! state: the blocks, the transactions in them, and the receipts they produced.
//! All three are write-once, read-by-name and never updated, which is why they
//! sit outside the state root rather than under it.
//!
//! | Segment | Row | Named by |
//! | --- | --- | --- |
//! | [`BLOCKS`] | the header, plus its body's transaction hashes | the block hash |
//! | [`TRANSACTIONS`] | the transaction, its recovered sender, and its block's hash | the transaction hash |
//! | [`RECEIPTS`] | the receipt | its transaction's hash |
//!
//! # Why the names matter
//!
//! A row's name *is* the hash reth would look it up by, so
//! `transaction_by_hash` is a direct read and Arkiv keeps no index of its own.
//! The two lookups that are not by hash — "the transactions of block N" and
//! "the receipts of block N" — come from the store's own
//! [`segment_range_of`](arkiv_interfaces::store::Store::segment_range_of),
//! which reports what one commit appended. A block is one commit, so that
//! range *is* the block's rows.
//!
//! # Encodings
//!
//! Consensus encodings, not bespoke ones: RLP for the header and the receipt,
//! EIP-2718 for the transaction. They are canonical, they are what the hashes
//! are taken over, and they are already the format every other Ethereum tool
//! expects — so a row stays readable without this crate.
//!
//! The one piece of non-consensus data is a transaction's **sender**. It is
//! recoverable from the signature, but recovery is expensive and reth asks for
//! senders in bulk (`senders_by_tx_range`), so it is stored beside the
//! transaction rather than recomputed per read.

use alloy_consensus::Header;
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256};
use alloy_rlp::Decodable;
use arkiv_interfaces::store::{SegmentKey, SegmentRow};
use reth_ethereum::{Receipt, TransactionSigned};

/// Block headers, named by block hash.
pub const BLOCKS: &str = "blocks";
/// Transactions with their senders, named by transaction hash.
pub const TRANSACTIONS: &str = "transactions";
/// Receipts, named by their transaction's hash.
pub const RECEIPTS: &str = "receipts";
/// Every segment and how many columns its rows have, as the encoders below
/// write them. A store that declares its segments up front, as GolemDB does,
/// takes them from here.
pub const ALL: [(&str, usize); 3] = [(BLOCKS, 2), (TRANSACTIONS, 3), (RECEIPTS, 1)];

/// One row to append, with the segment it belongs to and the name it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentWrite {
    pub segment: &'static str,
    pub key: Option<SegmentKey>,
    pub row: SegmentRow,
}

/// What a decoded `blocks` row holds: the header, and the hashes of the
/// transactions in that block, in order.
///
/// The hashes are what turn "block N's transactions" into reads of the
/// `transactions` segment, and they are what `block_body_indices` is derived
/// from. Storing them costs 32 bytes per transaction and saves keeping a
/// separate index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBlock {
    pub header: Header,
    pub transactions: Vec<B256>,
}

/// What a decoded `transactions` row holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTransaction {
    pub transaction: TransactionSigned,
    pub sender: Address,
    /// The block that included it.
    ///
    /// Carried on the row because the only other way to answer "which block
    /// was this transaction in" is an index from transaction to block, and
    /// avoiding exactly that index is why rows are named by hash at all. 32
    /// bytes per transaction buys `transaction_by_hash_with_meta`, which every
    /// `eth_getTransactionByHash` needs.
    pub block_hash: B256,
}

/// The rows one block contributes, in the order they must be appended.
///
/// Order is part of the contract: a row's ordinal comes from its position, and
/// the `transactions` and `receipts` segments are read in parallel by position
/// within a block. Appending them in transaction order keeps the *n*th
/// transaction and the *n*th receipt at the same offset in their block's range.
pub fn rows_for_block(
    header: &Header,
    transactions: &[TransactionSigned],
    senders: &[Address],
    receipts: &[Receipt],
    block_hash: B256,
) -> Result<Vec<SegmentWrite>, SegmentWriteError> {
    if transactions.len() != senders.len() {
        return Err(SegmentWriteError::SenderCountMismatch {
            transactions: transactions.len(),
            senders: senders.len(),
        });
    }
    if transactions.len() != receipts.len() {
        return Err(SegmentWriteError::ReceiptCountMismatch {
            transactions: transactions.len(),
            receipts: receipts.len(),
        });
    }

    let hashes: Vec<B256> = transactions.iter().map(|tx| *tx.hash()).collect();
    let mut writes = Vec::with_capacity(1 + transactions.len() * 2);
    writes.push(SegmentWrite {
        segment: BLOCKS,
        key: Some(SegmentKey(block_hash.0)),
        row: encode_block(header, &hashes),
    });
    for ((transaction, sender), hash) in transactions.iter().zip(senders).zip(&hashes) {
        writes.push(SegmentWrite {
            segment: TRANSACTIONS,
            key: Some(SegmentKey(hash.0)),
            row: encode_transaction(transaction, *sender, block_hash),
        });
    }
    for (receipt, hash) in receipts.iter().zip(&hashes) {
        writes.push(SegmentWrite {
            segment: RECEIPTS,
            key: Some(SegmentKey(hash.0)),
            row: encode_receipt(receipt),
        });
    }
    Ok(writes)
}

/// A block whose parts do not agree with each other. Refused rather than
/// written, because a block stored with the wrong number of receipts is
/// indistinguishable from a correct one afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentWriteError {
    SenderCountMismatch {
        transactions: usize,
        senders: usize,
    },
    ReceiptCountMismatch {
        transactions: usize,
        receipts: usize,
    },
}

impl core::fmt::Display for SegmentWriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SenderCountMismatch {
                transactions,
                senders,
            } => write!(
                f,
                "block has {transactions} transactions but {senders} recovered senders"
            ),
            Self::ReceiptCountMismatch {
                transactions,
                receipts,
            } => write!(
                f,
                "block has {transactions} transactions but {receipts} receipts"
            ),
        }
    }
}

impl core::error::Error for SegmentWriteError {}

// -- encoding -------------------------------------------------------------

/// Column 0 is the RLP header; column 1 is its transaction hashes, 32 bytes
/// each, concatenated.
fn encode_block(header: &Header, transactions: &[B256]) -> SegmentRow {
    let mut hashes = Vec::with_capacity(transactions.len() * 32);
    for hash in transactions {
        hashes.extend_from_slice(hash.as_slice());
    }
    vec![alloy_rlp::encode(header), hashes]
}

/// Column 0 is the EIP-2718 transaction, column 1 its 20-byte sender, column 2
/// the 32-byte hash of the block that included it.
fn encode_transaction(
    transaction: &TransactionSigned,
    sender: Address,
    block_hash: B256,
) -> SegmentRow {
    vec![
        transaction.encoded_2718(),
        sender.to_vec(),
        block_hash.to_vec(),
    ]
}

/// Column 0 is the receipt as plain RLP.
///
/// Not the EIP-2718 form, deliberately. reth's receipt trait requires RLP
/// `Encodable` and `Decodable` but *not* 2718 decoding, so a 2718 row could be
/// written and then not read back without pinning the receipt type — and
/// pinning it would spread through every provider impl that touches receipts.
/// Plain RLP is symmetric, generic, and round-trips the receipt's fields
/// exactly. The bloom is left out because it is derived from the logs.
fn encode_receipt(receipt: &Receipt) -> SegmentRow {
    vec![alloy_rlp::encode(receipt)]
}

/// A row that does not decode. Only reachable if a row was written by a
/// different version of this module, or the store handed back the wrong bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedRow(pub &'static str);

impl core::fmt::Display for MalformedRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "malformed segment row: {}", self.0)
    }
}

impl core::error::Error for MalformedRow {}

pub fn decode_block(row: &SegmentRow) -> Result<StoredBlock, MalformedRow> {
    let [header, hashes] = row.as_slice() else {
        return Err(MalformedRow("a blocks row has two columns"));
    };
    let header =
        Header::decode(&mut header.as_slice()).map_err(|_| MalformedRow("header is not RLP"))?;
    if !hashes.len().is_multiple_of(32) {
        return Err(MalformedRow("transaction hashes are not a multiple of 32"));
    }
    Ok(StoredBlock {
        header,
        transactions: hashes
            .chunks_exact(32)
            .map(|chunk| B256::from_slice(chunk))
            .collect(),
    })
}

pub fn decode_transaction(row: &SegmentRow) -> Result<StoredTransaction, MalformedRow> {
    let [transaction, sender, block_hash] = row.as_slice() else {
        return Err(MalformedRow("a transactions row has three columns"));
    };
    Ok(StoredTransaction {
        transaction: TransactionSigned::decode_2718(&mut transaction.as_slice())
            .map_err(|_| MalformedRow("transaction is not valid EIP-2718"))?,
        sender: Address::try_from(sender.as_slice())
            .map_err(|_| MalformedRow("sender is not 20 bytes"))?,
        block_hash: <[u8; 32]>::try_from(block_hash.as_slice())
            .map(B256::from)
            .map_err(|_| MalformedRow("block hash is not 32 bytes"))?,
    })
}

pub fn decode_receipt(row: &SegmentRow) -> Result<Receipt, MalformedRow> {
    let [receipt] = row.as_slice() else {
        return Err(MalformedRow("a receipts row has one column"));
    };
    Receipt::decode(&mut receipt.as_slice()).map_err(|_| MalformedRow("receipt is not RLP"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxType;

    /// A signed transaction, built by signing a minimal legacy transaction.
    fn a_transaction() -> TransactionSigned {
        use alloy_consensus::{SignableTransaction, TxLegacy};
        use alloy_primitives::{Signature, U256};
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: alloy_primitives::TxKind::Call(Address::repeat_byte(9)),
            value: U256::ZERO,
            input: Default::default(),
        };
        let signature = Signature::new(U256::from(1), U256::from(1), false);
        TransactionSigned::from(tx.into_signed(signature))
    }

    fn a_receipt() -> Receipt {
        Receipt {
            tx_type: TxType::Eip1559,
            success: true,
            cumulative_gas_used: 21_000,
            logs: vec![],
        }
    }

    /// A store that declares column counts refuses a row of any other width.
    #[test]
    fn all_matches_what_the_encoders_write() {
        let rows = [
            (BLOCKS, encode_block(&Header::default(), &[])),
            (
                TRANSACTIONS,
                encode_transaction(&a_transaction(), Address::ZERO, B256::ZERO),
            ),
            (RECEIPTS, encode_receipt(&a_receipt())),
        ];
        for (segment, row) in rows {
            let declared = ALL.iter().find(|(name, _)| *name == segment);
            assert_eq!(declared, Some(&(segment, row.len())));
        }
    }

    #[test]
    fn a_block_row_round_trips() {
        let header = Header {
            number: 7,
            gas_limit: 30_000_000,
            ..Default::default()
        };
        let hashes = vec![B256::repeat_byte(1), B256::repeat_byte(2)];
        let decoded = decode_block(&encode_block(&header, &hashes)).expect("decode");
        assert_eq!(decoded.header, header);
        assert_eq!(decoded.transactions, hashes);
    }

    #[test]
    fn a_block_with_no_transactions_round_trips() {
        // The common case on a quiet chain, and the one an off-by-one in the
        // hash packing would break.
        let decoded = decode_block(&encode_block(&Header::default(), &[])).expect("decode");
        assert!(decoded.transactions.is_empty());
    }

    #[test]
    fn a_transaction_row_carries_its_block() {
        // The block hash is what makes `transaction_by_hash_with_meta`
        // answerable without an index from transaction to block.
        let row = encode_transaction(
            &a_transaction(),
            Address::repeat_byte(3),
            B256::repeat_byte(7),
        );
        let decoded = decode_transaction(&row).expect("decode");
        assert_eq!(decoded.sender, Address::repeat_byte(3));
        assert_eq!(decoded.block_hash, B256::repeat_byte(7));
        assert_eq!(decoded.transaction, a_transaction());
    }

    #[test]
    fn a_receipt_row_round_trips() {
        let receipt = a_receipt();
        assert_eq!(
            decode_receipt(&encode_receipt(&receipt)).expect("decode"),
            receipt
        );
    }

    #[test]
    fn a_malformed_row_is_an_error_not_a_panic() {
        assert!(decode_block(&vec![]).is_err());
        assert!(decode_block(&vec![vec![0xff], vec![]]).is_err());
        assert!(decode_receipt(&vec![vec![0xff; 4]]).is_err());
        // A hash column that is not a whole number of hashes.
        let header = alloy_rlp::encode(Header::default());
        assert!(decode_block(&vec![header, vec![0u8; 33]]).is_err());
    }

    #[test]
    fn a_blocks_parts_must_agree() {
        let header = Header::default();
        // One receipt, no transactions: refused rather than written.
        let error = rows_for_block(&header, &[], &[], &[a_receipt()], B256::ZERO)
            .expect_err("mismatched counts are refused");
        assert!(matches!(
            error,
            SegmentWriteError::ReceiptCountMismatch {
                transactions: 0,
                receipts: 1
            }
        ));
    }

    #[test]
    fn an_empty_block_writes_only_its_header() {
        let writes =
            rows_for_block(&Header::default(), &[], &[], &[], B256::repeat_byte(9)).expect("rows");
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].segment, BLOCKS);
        assert_eq!(writes[0].key, Some(SegmentKey([9u8; 32])));
    }
}
