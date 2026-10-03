//! Entities read and written as native store records.
//!
//! # Read modes
//!
//! Unlike the indices, both [`ReadMode`]s are honourable here. `Store::get` accepts
//! either target, so `ViewWithOverlay` reads through the branch (seeing this view's
//! staged writes) and `ViewOnBase` reads the origin commit. Only ordered index
//! lookups are commit-only, because only they go through `query`.
//!
//! # The commitment
//!
//! [`GolemStateView::digest`] is `Store::branch_digest`, unchanged. That value becomes
//! the block's state root, which is the whole reason entities are laid out by
//! [`entity_records`] — a spec-level mapping — rather than by this crate.

use alloc::vec::Vec;

use arkiv_interfaces::entity::Entity;
use arkiv_interfaces::entity_records::{self, RecordError};
use arkiv_interfaces::primitives::EntityAddress;
use arkiv_interfaces::statemanager::{Commitment, EntityStore, EntityUpdates, ReadMode};
use arkiv_interfaces::store::{CellChange, CellName, Store, StoreError};

use crate::view::{GolemStateView, ViewError};

/// Why an entity operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityError {
    /// The store rejected the read or write.
    Store(StoreError),
    /// A record could not be mapped to or from an entity.
    Record(RecordError),
}

impl From<StoreError> for EntityError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<RecordError> for EntityError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}

/// Merge `next` onto `base` so the result is still net-against-base.
///
/// A field `next` does not mention keeps `base`'s value; one it does mention wins.
/// Two writes to the same entity in one view therefore collapse into the single
/// entry [`EntityStore::get_uncommitted_deltas`] promises, rather than two the
/// consumer would have to fold itself.
fn merge(base: &mut EntityUpdates, next: EntityUpdates) {
    base.entity = next.entity;
    // A delete is terminal: nothing after it in the same view can un-delete, because
    // a later create is a different entity lifecycle, not a field update.
    base.delete = next.delete;
    if next.creator.is_some() {
        base.creator = next.creator;
    }
    if next.owner.is_some() {
        base.owner = next.owner;
    }
    if next.created_at_block.is_some() {
        base.created_at_block = next.created_at_block;
    }
    if next.last_modified_at_block.is_some() {
        base.last_modified_at_block = next.last_modified_at_block;
    }
    if next.expires_at.is_some() {
        base.expires_at = next.expires_at;
    }
    if next.creation_flags.is_some() {
        base.creation_flags = next.creation_flags;
    }
    if next.content_type.is_some() {
        base.content_type = next.content_type;
    }
    if next.payload.is_some() {
        base.payload = next.payload;
    }
    if next.attributes.is_some() {
        base.attributes = next.attributes;
    }
}

impl<S: Store> GolemStateView<S> {
    /// Read an entity, or `None` if it does not exist at that read mode.
    fn get(&self, address: EntityAddress, read: ReadMode) -> Result<Option<Entity>, EntityError> {
        let record = self
            .store
            .get(
                self.target(read),
                entity_records::record_key(address),
                None,
                None,
            )?
            .into_value();
        record
            .as_ref()
            .map(entity_records::from_record)
            .transpose()
            .map_err(Into::into)
    }

    /// Apply `updates` to an entity, creating it if it does not exist.
    fn update(&mut self, updates: EntityUpdates) -> Result<(), EntityError> {
        let address = updates.entity;
        let key = entity_records::record_key(address);

        if updates.delete {
            // Absent is not an error: a view may tombstone something it also
            // created, and the net delta is still a deletion.
            match self.store.delete(self.branch, key, None, None) {
                Ok(_) | Err(StoreError::NotFound) => {}
                Err(error) => return Err(error.into()),
            }
            self.record(address, updates);
            return Ok(());
        }

        // Read-modify-write through the overlay, so two updates in one view compose.
        let existing = self.get(address, ReadMode::ViewWithOverlay)?;
        let mut entity = existing.clone().unwrap_or_default();
        updates.apply_to(&mut entity);
        let cells = entity_records::to_cells(&entity)?;

        match existing {
            None => {
                self.store.create(self.branch, key, cells, None)?;
            }
            Some(previous) => {
                // Patch rather than replace, and remove the cells that are no longer
                // there: an attribute dropped from the entity must leave the record,
                // or it stays queryable and the index answers with a ghost.
                let previous_cells = entity_records::to_cells(&previous)?;
                let mut changes: Vec<(CellName, CellChange)> = cells
                    .iter()
                    .map(|(name, cell)| (name.clone(), CellChange::Set(cell.clone())))
                    .collect();
                for (name, _) in &previous_cells {
                    if !cells.iter().any(|(kept, _)| kept == name) {
                        changes.push((name.clone(), CellChange::Remove));
                    }
                }
                self.store.patch(self.branch, key, None, changes, None)?;
            }
        }

        self.record(address, updates);
        Ok(())
    }

    fn record(&mut self, address: EntityAddress, updates: EntityUpdates) {
        self.staged
            .entry(address)
            .and_modify(|base| merge(base, updates.clone()))
            .or_insert(updates);
    }

    /// The net-against-base change per touched entity, ascending.
    fn uncommitted_deltas(&self) -> Vec<EntityUpdates> {
        self.staged.values().cloned().collect()
    }
}

impl<S: Store> EntityStore for GolemStateView<S> {
    type Error = ViewError;

    fn get_entity(
        &self,
        address: EntityAddress,
        read: ReadMode,
    ) -> Result<Option<Entity>, Self::Error> {
        self.get(address, read).map_err(Into::into)
    }

    fn update_entity(&mut self, updates: EntityUpdates) -> Result<(), Self::Error> {
        self.update(updates).map_err(Into::into)
    }

    fn get_uncommitted_deltas(&self) -> Result<Vec<EntityUpdates>, Self::Error> {
        Ok(self.uncommitted_deltas())
    }

    /// The writes are already on the branch; what clears is the delta log the
    /// index stores fold in, which only this view keeps.
    fn commit_store(&mut self) -> Result<Commitment, Self::Error> {
        self.staged.clear();
        self.shared_commitment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags};

    fn entity(key: u8) -> Entity {
        Entity {
            key: [key; 32],
            creator: [1; 20],
            owner: [2; 20],
            created_at_block: 1,
            last_modified_at_block: 1,
            expires_at: 100,
            creation_flags: CreationFlags::NONE,
            content_type: b"text/plain".to_vec(),
            payload: vec![key],
            attributes: vec![Attribute::new(b"level".to_vec(), AttributeValue::Int(10))],
        }
    }

    use arkiv_interfaces::store::ReadTarget;

    use crate::view::tests::view;

    #[test]
    fn a_created_entity_reads_back_through_the_overlay() {
        let mut view = view();
        view.update(EntityUpdates::create(entity(1)))
            .expect("create");
        let back = view.get([1; 32], ReadMode::ViewWithOverlay).expect("read");
        assert_eq!(back, Some(entity(1)));
    }

    #[test]
    fn base_reads_do_not_see_staged_writes() {
        // The distinction the two read modes exist for. `ViewOnBase` must answer as
        // of the origin commit even after this view has written.
        let mut view = view();
        view.update(EntityUpdates::create(entity(1)))
            .expect("create");
        assert_eq!(view.get([1; 32], ReadMode::ViewOnBase).expect("read"), None);
        assert!(
            view.get([1; 32], ReadMode::ViewWithOverlay)
                .expect("read")
                .is_some()
        );
    }

    #[test]
    fn a_partial_update_leaves_other_fields_alone() {
        let mut view = view();
        view.update(EntityUpdates::create(entity(1)))
            .expect("create");
        view.update(EntityUpdates {
            entity: [1; 32],
            owner: Some([9; 20]),
            ..Default::default()
        })
        .expect("update");
        let back = view
            .get([1; 32], ReadMode::ViewWithOverlay)
            .expect("read")
            .unwrap();
        assert_eq!(back.owner, [9; 20]);
        assert_eq!(back.payload, vec![1], "untouched fields survive");
        assert_eq!(back.attributes.len(), 1);
    }

    #[test]
    fn a_dropped_attribute_leaves_the_record() {
        // If a removed attribute's cell lingers, the index keeps matching it and the
        // query answers with an entity that no longer has the attribute.
        let mut view = view();
        view.update(EntityUpdates::create(entity(1)))
            .expect("create");
        view.update(EntityUpdates {
            entity: [1; 32],
            attributes: Some(vec![]),
            ..Default::default()
        })
        .expect("update");
        let back = view
            .get([1; 32], ReadMode::ViewWithOverlay)
            .expect("read")
            .unwrap();
        assert!(back.attributes.is_empty());

        let record = view
            .store
            .get(
                ReadTarget::Branch(view.branch),
                entity_records::record_key([1; 32]),
                None,
                None,
            )
            .expect("get")
            .into_value()
            .unwrap();
        assert!(
            record.cell("level").is_none(),
            "the cell itself must be gone"
        );
    }

    #[test]
    fn deleting_removes_the_record() {
        let mut view = view();
        view.update(EntityUpdates::create(entity(1)))
            .expect("create");
        view.update(EntityUpdates::deletion([1; 32]))
            .expect("delete");
        assert_eq!(
            view.get([1; 32], ReadMode::ViewWithOverlay).expect("read"),
            None
        );
    }

    #[test]
    fn deleting_something_absent_is_not_an_error() {
        let mut view = view();
        view.update(EntityUpdates::deletion([7; 32]))
            .expect("tombstone without a record");
    }

    #[test]
    fn two_writes_to_one_entity_collapse_into_one_delta() {
        // The trait promises one net-against-base entry per touched entity. Two
        // entries would make a consumer fold them itself, which is exactly the bug
        // the contract exists to prevent.
        let mut view = view();
        view.update(EntityUpdates::create(entity(1)))
            .expect("create");
        view.update(EntityUpdates {
            entity: [1; 32],
            owner: Some([9; 20]),
            ..Default::default()
        })
        .expect("update");

        let deltas = view.uncommitted_deltas();
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].owner, Some([9; 20]), "the later write wins");
        assert_eq!(
            deltas[0].payload,
            Some(vec![1]),
            "the earlier one is not lost"
        );
    }

    #[test]
    fn deltas_come_back_in_ascending_entity_order() {
        let mut view = view();
        for key in [3u8, 1, 2] {
            view.update(EntityUpdates::create(entity(key)))
                .expect("create");
        }
        let keys: Vec<u8> = view
            .uncommitted_deltas()
            .iter()
            .map(|d| d.entity[0])
            .collect();
        assert_eq!(keys, vec![1, 2, 3]);
    }

    #[test]
    fn a_delete_after_a_write_nets_to_a_deletion() {
        let mut view = view();
        view.update(EntityUpdates::create(entity(1)))
            .expect("create");
        view.update(EntityUpdates::deletion([1; 32]))
            .expect("delete");
        let deltas = view.uncommitted_deltas();
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].delete);
    }

    #[test]
    fn the_commitment_moves_only_when_content_does() {
        let mut view = view();
        let empty = view.digest().expect("digest");
        view.update(EntityUpdates::create(entity(1)))
            .expect("create");
        let one = view.digest().expect("digest");
        assert_ne!(empty, one, "a write must move the digest");

        // Writing the same content again is not a change.
        view.update(EntityUpdates::create(entity(1)))
            .expect("rewrite");
        assert_eq!(view.digest().expect("digest"), one);
    }

    #[test]
    fn an_unrepresentable_attribute_name_is_reported() {
        let mut view = view();
        let mut bad = entity(1);
        bad.attributes = vec![Attribute::new(vec![0xff, 0xfe], AttributeValue::Int(1))];
        assert_eq!(
            view.update(EntityUpdates::create(bad)),
            Err(EntityError::Record(RecordError::AttributeNameNotUtf8))
        );
    }

    #[test]
    fn a_string_attribute_survives_the_round_trip() {
        let mut view = view();
        let mut with_str = entity(1);
        with_str.attributes = vec![Attribute::new(
            b"name".to_vec(),
            AttributeValue::Str(String::from("bob")),
        )];
        view.update(EntityUpdates::create(with_str.clone()))
            .expect("create");
        assert_eq!(
            view.get([1; 32], ReadMode::ViewWithOverlay).expect("read"),
            Some(with_str)
        );
    }
}
