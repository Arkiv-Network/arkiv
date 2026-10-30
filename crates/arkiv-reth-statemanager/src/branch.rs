//! [`OwnedBranch`]: a store branch that is discarded unless something takes it.
//!
//! A branch holds memory in the store until it is committed or discarded, and
//! GolemDB reclaims an abandoned one only when something touches it again --
//! which nothing does. So a branch nobody released was a leak, and plenty of
//! executions end without releasing theirs: `eth_call`, `eth_estimateGas`,
//! payload prewarming, a build reth aborts or cancels. Owning the branch makes
//! dropping it the release.

use arkiv_interfaces::store::{BranchId, CommitId, SealedCommit, Store, StoreError, StoreExt};

use crate::host::HostStore;

/// An open branch over the store's head, discarded on drop unless committed.
#[derive(Debug)]
#[must_use = "dropping an OwnedBranch discards it"]
pub struct OwnedBranch {
    store: HostStore,
    /// `None` once committed.
    id: Option<BranchId>,
}

impl OwnedBranch {
    /// Open a branch over the store's head.
    pub fn begin(store: &HostStore) -> Result<Self, StoreError> {
        let id = store.begin(None)?;
        Ok(Self {
            store: store.clone(),
            id: Some(id),
        })
    }

    pub fn id(&self) -> BranchId {
        self.id
            .expect("an OwnedBranch holds its id until it is consumed")
    }

    /// Freeze the branch and compute its roots. It stays owned: sealing writes
    /// nothing, and the branch is still committed or discarded afterwards.
    pub fn seal(&self) -> Result<SealedCommit, StoreError> {
        self.store.seal(self.id())
    }

    /// Commit the branch as the new head, tagged `tag`. On failure the branch
    /// is still owned, so it is discarded on the way out.
    pub fn commit_tagged(mut self, tag: [u8; 32]) -> Result<CommitId, StoreError> {
        let commit = self.store.commit_tagged(self.id(), tag)?;
        self.id = None;
        Ok(commit)
    }
}

impl Drop for OwnedBranch {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            // A branch a later commit made stale is refused with
            // `HandleInvalid`, but discarding it still releases it.
            let _ = self.store.discard(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_interfaces::store::reference::MemStore;
    use std::sync::Arc;

    fn store() -> HostStore {
        let store: HostStore = Arc::new(MemStore::new());
        store
            .commit_tagged(store.begin(None).expect("begin"), [0; 32])
            .expect("tag genesis");
        store
    }

    #[test]
    fn dropping_discards_the_branch() {
        let store = store();
        let id = OwnedBranch::begin(&store).expect("begin").id();
        assert_eq!(store.discard(id), Err(StoreError::HandleInvalid));
    }

    #[test]
    fn a_sealed_branch_is_still_discarded_on_drop() {
        let store = store();
        let branch = OwnedBranch::begin(&store).expect("begin");
        branch.seal().expect("seal");
        let id = branch.id();
        drop(branch);
        assert_eq!(store.discard(id), Err(StoreError::HandleInvalid));
    }

    #[test]
    fn committing_consumes_the_branch() {
        let store = store();
        let branch = OwnedBranch::begin(&store).expect("begin");
        let commit = branch.commit_tagged([1; 32]).expect("commit");
        assert_eq!(store.head(), commit);
        assert_eq!(store.commit_by_tag([1; 32]), Ok(Some(commit)));
    }
}
