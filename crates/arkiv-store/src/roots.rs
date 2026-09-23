//! The top node: the three trie roots, RLP-encoded and stored in the node
//! store under their keccak, which is the database root.

use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use arkiv_trie::{EMPTY_ROOT_HASH, NodeReader, NodeSink};

/// The database root of an empty database. The anchor slot reads as zero
/// before anything is written, and [`DbRoots::load`] maps zero here too.
pub const EMPTY_DB_ROOT: B256 = B256::ZERO;

/// The three roots behind one database root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct DbRoots {
    pub entities: B256,
    pub nonces: B256,
    pub indexes: B256,
}

impl Default for DbRoots {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl DbRoots {
    pub const EMPTY: Self = Self {
        entities: EMPTY_ROOT_HASH,
        nonces: EMPTY_ROOT_HASH,
        indexes: EMPTY_ROOT_HASH,
    };

    pub fn is_empty(&self) -> bool {
        *self == Self::EMPTY
    }

    /// The database root: `keccak(rlp([entities, nonces, indexes]))`, or
    /// [`EMPTY_DB_ROOT`] for the empty database.
    pub fn root(&self) -> B256 {
        if self.is_empty() {
            return EMPTY_DB_ROOT;
        }
        keccak256(self.rlp())
    }

    fn rlp(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 * 33 + 2);
        self.encode(&mut out);
        out
    }

    /// Store the top node and return the database root.
    pub fn store<S: NodeSink>(&self, store: &mut S) -> B256 {
        let root = self.root();
        if root != EMPTY_DB_ROOT {
            store.put_node(root, self.rlp());
        }
        root
    }

    /// Read the top node behind `root`. `None` when the node store lacks it.
    pub fn load<S: NodeReader>(store: &S, root: B256) -> Result<Option<Self>, S::Error> {
        if root == EMPTY_DB_ROOT {
            return Ok(Some(Self::EMPTY));
        }
        let Some(rlp) = store.node(&root)? else {
            return Ok(None);
        };
        Ok(Self::decode(&mut rlp.as_slice()).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_trie::MemNodeStore;

    #[test]
    fn empty_roots_hash_to_zero_and_round_trip() {
        let mut store = MemNodeStore::new();
        assert_eq!(DbRoots::EMPTY.store(&mut store), EMPTY_DB_ROOT);
        assert!(store.is_empty());
        assert_eq!(
            DbRoots::load(&store, EMPTY_DB_ROOT).unwrap(),
            Some(DbRoots::EMPTY)
        );

        let roots = DbRoots {
            entities: B256::repeat_byte(1),
            nonces: B256::repeat_byte(2),
            indexes: B256::repeat_byte(3),
        };
        let root = roots.store(&mut store);
        assert_ne!(root, EMPTY_DB_ROOT);
        assert_eq!(DbRoots::load(&store, root).unwrap(), Some(roots));
        assert_eq!(DbRoots::load(&store, B256::repeat_byte(9)).unwrap(), None);
    }
}
