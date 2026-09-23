//! In-memory stand-ins for tests: a base with balances, nonces and an anchor
//! slot, and the view over it with an in-memory node store.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use arkiv_interfaces::statemanager::BlockRef;
use arkiv_store::{MemNodeStore, SharedMemNodeStore, Staging};

use crate::MptStateView;
use crate::seams::{AnchorAccess, BalanceAccess, NonceAccess};

/// An in-memory base implementing every raw seam.
#[derive(Debug, Default, Clone)]
pub struct MemBase {
    pub balances: HashMap<Address, U256>,
    pub nonces: HashMap<Address, u64>,
    pub db_root: B256,
}

impl BalanceAccess for MemBase {
    type Error = core::convert::Infallible;

    fn get_balance(&mut self, addr: Address) -> Result<U256, Self::Error> {
        Ok(self.balances.get(&addr).copied().unwrap_or_default())
    }

    fn set_balance(&mut self, addr: Address, balance: U256) -> Result<(), Self::Error> {
        self.balances.insert(addr, balance);
        Ok(())
    }
}

impl NonceAccess for MemBase {
    type Error = core::convert::Infallible;

    fn get_nonce(&mut self, addr: Address) -> Result<u64, Self::Error> {
        Ok(self.nonces.get(&addr).copied().unwrap_or_default())
    }

    fn set_nonce(&mut self, addr: Address, nonce: u64) -> Result<(), Self::Error> {
        self.nonces.insert(addr, nonce);
        Ok(())
    }
}

impl AnchorAccess for MemBase {
    type Error = core::convert::Infallible;

    fn db_root(&mut self) -> Result<B256, Self::Error> {
        Ok(self.db_root)
    }

    fn set_db_root(&mut self, root: B256) -> Result<(), Self::Error> {
        self.db_root = root;
        Ok(())
    }
}

/// The shared in-memory node store a test's views write into.
pub type MemNodes = Arc<SharedMemNodeStore>;

pub fn mem_nodes() -> MemNodes {
    Arc::new(SharedMemNodeStore::new())
}

/// A view over a [`MemBase`] with its nodes staged over an in-memory store.
pub type MemView = MptStateView<MemBase, Staging<MemNodes>>;

/// Open a view over `base` and `nodes` at `block`.
pub fn mem_view(base: MemBase, nodes: MemNodes, block: BlockRef) -> MemView {
    MptStateView::new(base, Staging::new(nodes), block).expect("in-memory view opens")
}

/// Commit the view's staged nodes into its store and hand back the base and
/// the store, so the next view sees what this one wrote.
pub fn settle(view: MemView) -> (MemBase, MemNodes) {
    let (base, staging) = view.into_parts();
    let (staged, nodes): (MemNodeStore, MemNodes) = staging.into_parts();
    arkiv_store::NodeStore::flush(&nodes, staged).expect("in-memory flush");
    (base, nodes)
}
