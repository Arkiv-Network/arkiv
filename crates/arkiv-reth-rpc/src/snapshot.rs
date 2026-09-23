//! [`SnapshotView`] — the reth **read-path** bridge.
//!
//! The mirror of the write path's `WriteOverlay` (arkiv-reth-statemanager):
//! read the anchor slot from a committed reth state snapshot
//! ([`StateProviderBox`]) to learn the database root as of that block, then
//! open the Arkiv database at that root. Every root a block ever committed
//! stays in the node store, so a historical snapshot answers historical reads
//! and queries with nothing more than its anchor slot.

use std::sync::Arc;

use arkiv_store::{ARKIV_ROOT_ACCOUNT, ARKIV_ROOT_SLOT, ArkivDb, B256, DbRoots, DbView};
use reth_storage_api::StateProviderBox;

/// The Arkiv database as of one reth state snapshot.
pub struct SnapshotView {
    db: Arc<ArkivDb>,
    roots: DbRoots,
    root: B256,
}

impl SnapshotView {
    /// Open the database at the root `state`'s anchor slot names.
    pub fn open(state: &StateProviderBox, db: Arc<ArkivDb>) -> eyre::Result<Self> {
        let slot = state
            .storage(ARKIV_ROOT_ACCOUNT, ARKIV_ROOT_SLOT.into())
            .map_err(|e| eyre::eyre!("read the anchor slot: {e:?}"))?;
        let root = slot
            .map(|v| B256::from(v.to_be_bytes::<32>()))
            .unwrap_or_default();
        let roots = DbRoots::load(&db, root)
            .map_err(|e| eyre::eyre!("read the database root {root}: {e}"))?
            .ok_or_else(|| eyre::eyre!("database root {root} is not in the node store"))?;
        Ok(Self { db, roots, root })
    }

    /// The database root this view answers for.
    pub fn root(&self) -> B256 {
        self.root
    }

    pub fn view(&self) -> DbView<'_, Arc<ArkivDb>> {
        DbView::at(&self.db, self.roots)
    }
}
