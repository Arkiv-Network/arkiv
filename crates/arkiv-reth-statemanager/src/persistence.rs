//! Arkiv's persistence service: the engine's blocks, committed to the store.
//!
//! reth's engine keeps recent blocks in memory and hands them to a persistence
//! service once it considers them settled. The stock service writes them into
//! reth's own database. This one commits them to the Arkiv Store instead, and
//! reth's service does not run — see
//! `EngineProviderBuilder::engine_persistence`.
//!
//! # It acknowledges; it does not write
//!
//! Arkiv's commit *is* the durable write. State and the block's rows go into
//! the store together, in one commit, at canonicalisation -- which is also
//! when they have to happen, because the next block is built on the committed
//! state of this one.
//!
//! So this service writes nothing. It answers reth's question -- how far is
//! the chain durable? -- from the store's own head, which is the only record
//! there is. That is what makes it one record: reth keeps no second copy to
//! fall out of step with.
//!
//! The answer has to be honest. Telling reth a block is persisted is what
//! lets it drop that block from memory, so this reports the store's head
//! rather than the height it was asked about, and stops short of any block
//! below that head the store holds no commit for. A block the store has not
//! committed is simply not acknowledged, and reth holds on to it.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockNumHash;
use reth_engine_tree::persistence::{PersistenceAction, PersistenceHandle, PersistenceResult};
use reth_ethereum::EthPrimitives;
use reth_ethereum::primitives::SealedHeader;
use tracing::{debug, error};

use crate::host::{BlockSeals, HostStore};

/// Start the service and return the handle the engine sends through.
///
/// The thread lives as long as the channel: when the engine drops its handle,
/// `recv` ends and the loop returns.
pub fn spawn(store: HostStore, seals: Arc<BlockSeals>) -> PersistenceHandle<EthPrimitives> {
    let _ = &seals;
    let (sender, receiver) = channel();
    std::thread::Builder::new()
        .name("arkiv-persistence".to_owned())
        .spawn(move || run(&store, &receiver))
        .expect("spawn the arkiv persistence thread");
    PersistenceHandle::new(sender)
}

fn run(store: &HostStore, receiver: &Receiver<PersistenceAction>) {
    while let Ok(action) = receiver.recv() {
        match action {
            PersistenceAction::SaveBlocks(input, reply) => {
                let asked = input.last_block();
                match durable(store) {
                    Ok(Some(durable)) => {
                        // The reply is permission to forget, so it is capped
                        // at what reth asked about. The store commits a block
                        // as it is canonicalised and so runs ahead of the
                        // question; answering with the store's own head would
                        // release the blocks reth is still standing on --
                        // including the canonical head, whose parent the next
                        // payload needs.
                        let acked = if durable.number < asked.number {
                            // Behind what reth hoped for. Reporting the lower
                            // number is what keeps the uncommitted blocks in
                            // its memory rather than dropped as settled.
                            debug!(
                                target: "arkiv-reth",
                                asked = asked.number,
                                durable = durable.number,
                                "the store has not committed this far yet",
                            );
                            durable
                        } else {
                            asked
                        };
                        let acked = committed_through(
                            store,
                            input
                                .persist_rest_blocks()
                                .iter()
                                .map(|block| block.recovered_block().sealed_header()),
                            acked,
                        );
                        let _ = reply.send(PersistenceResult {
                            last_block: acked,
                            last_state_trie_block: acked,
                            commit_duration: None,
                        });
                    }
                    Ok(None) => {
                        // Nothing committed yet. The engine blocks until it
                        // hears back, so silence is not an option: answer with
                        // the empty height, which says nothing may be dropped.
                        debug!(target: "arkiv-reth", "the store holds no block yet");
                        let _ = reply.send(PersistenceResult {
                            last_block: BlockNumHash::default(),
                            last_state_trie_block: BlockNumHash::default(),
                            commit_duration: None,
                        });
                    }
                    Err(error) => {
                        error!(
                            target: "arkiv-reth",
                            %error,
                            "cannot tell how far the arkiv store is committed",
                        );
                        let _ = reply.send(PersistenceResult {
                            last_block: BlockNumHash::default(),
                            last_state_trie_block: BlockNumHash::default(),
                            commit_duration: None,
                        });
                    }
                }
            }
            PersistenceAction::RemoveBlocksAbove(height, reply) => {
                // Arkiv commits only blocks that cannot be reorged, so nothing
                // should ever ask for one back, and the store cannot rewind.
                // Answered with where the store actually stands, which is the
                // truthful reply whether or not reth likes it.
                error!(
                    target: "arkiv-reth",
                    height,
                    "asked to remove committed blocks; the arkiv store cannot rewind",
                );
                if let Ok(Some(durable)) = durable(store) {
                    let _ = reply.send(PersistenceResult {
                        last_block: durable,
                        last_state_trie_block: durable,
                        commit_duration: None,
                    });
                }
            }
            // Forkchoice markers, not block data. The store derives both from
            // its own head, so there is nothing to record.
            PersistenceAction::SaveFinalizedBlock(_) | PersistenceAction::SaveSafeBlock(_) => {}
        }
    }
    debug!(target: "arkiv-reth", "arkiv persistence service stopped");
}

/// How far reth may forget: `acked`, or the parent of the first of `blocks`
/// at or below it that the store holds no commit for.
///
/// The store's head is not proof enough. Adoption commits blocks one at a
/// time, and a block it skipped -- an adoption that failed, a block that never
/// sealed, a notification adopted only at its tip -- leaves a gap below a head
/// that has moved on. Acknowledging across that gap would let reth drop a block
/// the store does not have, and with it the only copy of its header, body and
/// receipts. So every block must have a commit tagged with its own hash; the
/// first that does not stops the acknowledgement, and it and every block after
/// it stay in reth's memory.
fn committed_through<'a>(
    store: &HostStore,
    blocks: impl IntoIterator<Item = &'a SealedHeader>,
    acked: BlockNumHash,
) -> BlockNumHash {
    for block in blocks
        .into_iter()
        .take_while(|block| block.number <= acked.number)
    {
        match store.commit_by_tag(block.hash().0) {
            Ok(Some(_)) => continue,
            Ok(None) => error!(
                target: "arkiv-reth",
                number = block.number,
                hash = %block.hash(),
                "the arkiv store has no commit for this block; keeping it and every block after it in memory",
            ),
            Err(error) => error!(
                target: "arkiv-reth",
                number = block.number,
                hash = %block.hash(),
                ?error,
                "cannot tell whether the arkiv store committed this block; keeping it and every block after it in memory",
            ),
        }
        return block.parent_num_hash();
    }
    acked
}

/// How far the store is committed: the block its head commit holds.
///
/// The head commit is tagged with its block hash, which names the row in the
/// `blocks` segment whose header carries the number. A head commit with no
/// tag is not a block -- the genesis allocation is one -- and so nothing is
/// durable yet.
fn durable(store: &HostStore) -> eyre::Result<Option<BlockNumHash>> {
    let head = store.head();
    let Some(tag) = store
        .tag_of(head)
        .map_err(|error| eyre::eyre!("tag_of({}): {error:?}", head.0))?
    else {
        return Ok(None);
    };
    let hash = alloy_primitives::B256::from(tag);
    let row = store
        .segment_get(
            crate::segments::BLOCKS,
            arkiv_interfaces::store::SegmentAddress::Key(arkiv_interfaces::store::SegmentKey(tag)),
        )
        .map_err(|error| eyre::eyre!("segment_get(blocks, {hash}): {error:?}"))?;
    let Some(row) = row else { return Ok(None) };
    let block = crate::segments::decode_block(&row)
        .map_err(|error| eyre::eyre!("decode the head block row: {error}"))?;
    Ok(Some(BlockNumHash::new(block.header.number(), hash)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use arkiv_interfaces::store::reference::MemStore;

    /// Blocks 1 to `n`, each naming the one before as its parent.
    fn chain(n: u64) -> Vec<SealedHeader> {
        let mut parent = alloy_primitives::B256::ZERO;
        (1..=n)
            .map(|number| {
                let header = SealedHeader::seal_slow(Header {
                    number,
                    parent_hash: parent,
                    ..Header::default()
                });
                parent = header.hash();
                header
            })
            .collect()
    }

    /// A store holding a commit tagged with the hash of each of `blocks`.
    fn store_with(blocks: &[&SealedHeader]) -> HostStore {
        let store: HostStore = Arc::new(MemStore::new());
        for block in blocks {
            let branch = store.begin(None).expect("begin");
            store.commit_tagged(branch, block.hash().0).expect("commit");
        }
        store
    }

    #[test]
    fn a_fully_committed_range_is_acknowledged_in_full() {
        let chain = chain(3);
        let store = store_with(&[&chain[0], &chain[1], &chain[2]]);
        let asked = chain[2].num_hash();
        assert_eq!(committed_through(&store, &chain, asked), asked);
    }

    #[test]
    fn a_range_with_an_uncommitted_block_is_acknowledged_only_up_to_the_gap() {
        let chain = chain(3);
        // Block 2 was never adopted; the store's head is block 3 regardless.
        let store = store_with(&[&chain[0], &chain[2]]);
        assert_eq!(
            committed_through(&store, &chain, chain[2].num_hash()),
            chain[0].num_hash(),
        );
    }

    #[test]
    fn a_range_whose_first_block_is_uncommitted_acknowledges_only_its_parent() {
        let chain = chain(3);
        let store = store_with(&[&chain[1], &chain[2]]);
        assert_eq!(
            committed_through(&store, &chain, chain[2].num_hash()),
            BlockNumHash::new(0, alloy_primitives::B256::ZERO),
        );
    }
}
