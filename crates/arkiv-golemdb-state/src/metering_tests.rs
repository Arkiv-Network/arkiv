//! The receipts the view's entity and minting-nonce calls return.
//!
//! `MemStore` prices nothing, so every cost it reports is zero and a sum that
//! dropped a receipt would look the same as one that kept it. [`Priced`] wraps
//! it and gives each call kind its own power of ten: a total then spells out
//! which calls it is made of, one digit per kind. Write details come from
//! `MemStore` unchanged, so they are checked against [`WriteDetails::between`].

use alloc::vec;

use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags, Entity};
use arkiv_interfaces::entity_records;
use arkiv_interfaces::primitives::{EntityCreationNonce, UserAddress, UserBalance};
use arkiv_interfaces::statemanager::{
    AccountBalancesStore, AccountNoncesStore, BlockRef, EntityCreationNoncesStore, EntityStore,
    EntityUpdates, ReadMode, SessionId,
};
use arkiv_interfaces::store::priced::{PRICED_AT, Priced, Prices, SCHEDULE};
use arkiv_interfaces::store::reference::MemStore;
use arkiv_interfaces::store::{CostUnits, Receipt, Store, StoreError, WriteDetails};

use crate::entities::EntityError;
use crate::{GolemStateView, ViewError};

const ALICE: UserAddress = [0xaa; 20];

const PRICES: Prices = Prices::DISTINCT;

type PricedView = GolemStateView<Priced<MemStore>>;

fn view_priced(prices: Prices) -> PricedView {
    view_over(Priced::new(MemStore::new(), prices))
}

fn view_over(store: Priced<MemStore>) -> PricedView {
    let origin = store.head();
    let branch = store.begin(Some(origin)).unwrap();
    GolemStateView::new(
        store,
        branch,
        origin,
        BlockRef::new(10, [0xBB; 32]),
        SessionId([7; 16]),
    )
}

fn view() -> PricedView {
    view_priced(PRICES)
}

fn entity(key: u8, level: i32) -> Entity {
    Entity {
        key: [key; 32],
        creator: ALICE,
        owner: ALICE,
        created_at: 1,
        updated_at: 1,
        expires_at: 100,
        creation_flags: CreationFlags::NONE,
        attributes: vec![
            Attribute::unindexed(b"body".to_vec(), AttributeValue::Bytes(vec![key])),
            Attribute::new(b"level".to_vec(), AttributeValue::Int(level)),
        ],
    }
}

/// The effects of an entity going from `before` to `after`, by the shared rule.
fn details(before: Option<&Entity>, after: Option<&Entity>) -> WriteDetails {
    let cells = |entity: Option<&Entity>| {
        entity
            .map(|entity| entity_records::to_cells(entity).unwrap())
            .unwrap_or_default()
    };
    let (before, after) = (cells(before), cells(after));
    WriteDetails::between(
        before.iter().map(|(name, cell)| (name, cell)),
        after.iter().map(|(name, cell)| (name, cell)),
    )
    .unwrap()
}

#[test]
fn a_create_reports_its_existence_read_and_its_write() {
    let mut view = view();
    let created = view
        .update_entity(EntityUpdates::create(entity(1, 10)))
        .unwrap();
    assert_eq!(created.receipt.cost, CostUnits(PRICES.get + PRICES.create));
    assert_eq!(created.receipt.details, details(None, Some(&entity(1, 10))));
}

#[test]
fn an_update_reports_its_read_and_its_patch() {
    let mut view = view();
    view.update_entity(EntityUpdates::create(entity(1, 10)))
        .unwrap();
    let updated = view
        .update_entity(EntityUpdates {
            attributes: Some(entity(1, 11).attributes),
            ..EntityUpdates::create(entity(1, 10))
        })
        .unwrap();
    assert_eq!(updated.receipt.cost, CostUnits(PRICES.get + PRICES.patch));
    assert_eq!(
        updated.receipt.details,
        details(Some(&entity(1, 10)), Some(&entity(1, 11)))
    );
    assert_eq!(
        updated.receipt.details.cells_updated, 1,
        "only `level` moved"
    );
}

#[test]
fn a_delete_reports_the_cells_it_removed() {
    let mut view = view();
    view.update_entity(EntityUpdates::create(entity(1, 10)))
        .unwrap();
    let deleted = view
        .update_entity(EntityUpdates::deletion([1; 32]))
        .unwrap();
    assert_eq!(deleted.receipt.cost, CostUnits(PRICES.delete));
    assert_eq!(deleted.receipt.details, details(Some(&entity(1, 10)), None));
}

/// A reverted transaction loses its writes but not what they cost: GolemDB
/// charges rolled-back work, and the receipts are the caller's, not the view's.
#[test]
fn a_discarded_frame_keeps_what_it_spent() {
    let mut view = view();
    view.begin_frame().unwrap();
    let created = view
        .update_entity(EntityUpdates::create(entity(1, 10)))
        .unwrap();
    let minted = view.fetch_increment_entity_creation_nonce(ALICE).unwrap();
    let mut spent = created.receipt;
    spent.accumulate(&minted.receipt);
    view.discard_frame().unwrap();

    // One read and one create each: the entity, and ALICE's account record,
    // which the nonce increment creates because she has none yet.
    assert_eq!(
        spent.cost,
        CostUnits(PRICES.get + PRICES.create + PRICES.get + PRICES.create)
    );
    assert_eq!(
        view.get_entity([1; 32], ReadMode::ViewWithOverlay)
            .unwrap()
            .value,
        None
    );
    assert_eq!(
        view.get_entity_creation_nonce(ALICE, ReadMode::ViewWithOverlay)
            .unwrap()
            .value,
        EntityCreationNonce::new(0)
    );
    assert!(view.get_uncommitted_deltas().unwrap().is_empty());
}

/// Until failed calls carry receipts, the `NotFound` the delete swallows has
/// nothing to report.
#[test]
fn deleting_an_absent_entity_reports_nothing() {
    let mut view = view();
    let deleted = view
        .update_entity(EntityUpdates::deletion([1; 32]))
        .unwrap();
    assert_eq!(deleted.receipt, Receipt::default());
}

#[test]
fn a_read_that_finds_nothing_still_costs_its_read() {
    let view = view();
    let read = view.get_entity([1; 32], ReadMode::ViewWithOverlay).unwrap();
    assert_eq!(read.value, None);
    assert_eq!(read.receipt.cost, CostUnits(PRICES.get));
    assert_eq!(read.receipt.details, WriteDetails::ZERO);
}

/// A full read always carries the record's size, so an entity record without
/// one is a store fault, on both entity reads, never an unknown size.
#[test]
fn an_entity_record_without_its_size_is_a_store_fault() {
    let mut view = view_over(Priced::new(MemStore::new(), PRICES).without_meta());
    view.update_entity(EntityUpdates::create(entity(1, 10)))
        .unwrap();

    assert_eq!(
        view.get_entity_with_meta([1; 32], ReadMode::ViewWithOverlay)
            .map(|read| read.value),
        Err(ViewError::Entity(EntityError::Store(StoreError::Internal))),
    );
    assert_eq!(
        view.get_entity([1; 32], ReadMode::ViewWithOverlay)
            .map(|read| read.value),
        Err(ViewError::Entity(EntityError::Store(StoreError::Internal))),
        "the plain read fails too: it is the same read"
    );
}

#[test]
fn a_read_that_finds_an_entity_costs_its_read() {
    let mut view = view();
    view.update_entity(EntityUpdates::create(entity(1, 10)))
        .unwrap();
    let read = view.get_entity([1; 32], ReadMode::ViewWithOverlay).unwrap();
    assert_eq!(read.value, Some(entity(1, 10)));
    assert_eq!(read.receipt.cost, CostUnits(PRICES.get));
}

#[test]
fn reading_the_minting_nonce_costs_one_read() {
    let view = view();
    let read = view
        .get_entity_creation_nonce(ALICE, ReadMode::ViewWithOverlay)
        .unwrap();
    assert_eq!(read.value, EntityCreationNonce::new(0));
    assert_eq!(read.receipt.cost, CostUnits(PRICES.get));
}

/// One account read, then a create for a new account or a patch once it exists.
#[test]
fn advancing_the_minting_nonce_reports_one_read_and_one_write() {
    let mut view = view();
    let first = view.fetch_increment_entity_creation_nonce(ALICE).unwrap();
    assert_eq!(first.value, EntityCreationNonce::new(0));
    assert_eq!(first.receipt.cost, CostUnits(PRICES.get + PRICES.create));
    assert_eq!(first.receipt.details.cells_created, 1);

    let second = view.fetch_increment_entity_creation_nonce(ALICE).unwrap();
    assert_eq!(second.value, EntityCreationNonce::new(1));
    assert_eq!(second.receipt.cost, CostUnits(PRICES.get + PRICES.patch));
    assert_eq!(second.receipt.details.cells_updated, 1);
}

/// A sum that does not fit saturates at `u64::MAX` rather than failing the
/// call: the executor's checked meter turns any cost that large into out of
/// gas, a revert, where an error here would be a store fault that aborts the
/// block. The write itself lands as usual.
#[test]
fn a_cost_that_does_not_fit_saturates() {
    let prices = Prices {
        get: u64::MAX,
        ..PRICES
    };

    let mut entities = view_priced(prices);
    let created = entities
        .update_entity(EntityUpdates::create(entity(1, 10)))
        .unwrap();
    assert_eq!(created.receipt.cost, CostUnits(u64::MAX));
    assert_eq!(
        entities
            .get_entity([1; 32], ReadMode::ViewWithOverlay)
            .unwrap()
            .value,
        Some(entity(1, 10))
    );

    let mut accounts = view_priced(prices);
    let incremented = accounts
        .fetch_increment_entity_creation_nonce(ALICE)
        .unwrap();
    assert_eq!(incremented.value, EntityCreationNonce::new(0));
    assert_eq!(incremented.receipt.cost, CostUnits(u64::MAX));
}

#[test]
fn the_nonce_receipt_sum_reaches_the_limit_and_saturates_beyond() {
    let mut exact = view_priced(Prices {
        get: u64::MAX - 1,
        create: 1,
        ..PRICES
    });
    let incremented = exact.fetch_increment_entity_creation_nonce(ALICE).unwrap();
    assert_eq!(incremented.value, EntityCreationNonce::new(0));
    assert_eq!(incremented.receipt.cost, CostUnits(u64::MAX));

    let mut overflowing = view_priced(Prices {
        get: u64::MAX - 1,
        create: 2,
        ..PRICES
    });
    let incremented = overflowing
        .fetch_increment_entity_creation_nonce(ALICE)
        .unwrap();
    assert_eq!(incremented.value, EntityCreationNonce::new(0));
    assert_eq!(incremented.receipt.cost, CostUnits(u64::MAX));
}

#[test]
fn pricing_metadata_survives_reads_writes_and_aggregation() {
    let mut view = view();
    let receipts = [
        view.get_entity([1; 32], ReadMode::ViewWithOverlay)
            .unwrap()
            .receipt,
        view.get_entity_creation_nonce(ALICE, ReadMode::ViewWithOverlay)
            .unwrap()
            .receipt,
        view.update_entity(EntityUpdates::create(entity(1, 10)))
            .unwrap()
            .receipt,
        view.get_entity([1; 32], ReadMode::ViewWithOverlay)
            .unwrap()
            .receipt,
        view.update_entity(EntityUpdates::create(entity(1, 11)))
            .unwrap()
            .receipt,
        view.update_entity(EntityUpdates::deletion([1; 32]))
            .unwrap()
            .receipt,
        view.fetch_increment_entity_creation_nonce(ALICE)
            .unwrap()
            .receipt,
        view.get_entity_creation_nonce(ALICE, ReadMode::ViewWithOverlay)
            .unwrap()
            .receipt,
        view.fetch_increment_entity_creation_nonce(ALICE)
            .unwrap()
            .receipt,
    ];
    for receipt in receipts {
        assert_eq!(receipt.schedule, SCHEDULE);
        assert_eq!(receipt.priced_at, Some(PRICED_AT));
    }
}

#[test]
fn no_op_and_attribute_removal_report_only_their_actual_effects() {
    let mut view = view();
    let before = entity(1, 10);
    view.update_entity(EntityUpdates::create(before.clone()))
        .unwrap();
    let unchanged = view
        .update_entity(EntityUpdates::create(before.clone()))
        .unwrap();
    assert_eq!(unchanged.receipt.cost, CostUnits(PRICES.get + PRICES.patch));
    assert_eq!(unchanged.receipt.details, WriteDetails::ZERO);

    let mut after = before.clone();
    after.attributes.clear();
    let removed = view
        .update_entity(EntityUpdates::create(after.clone()))
        .unwrap();
    assert_eq!(removed.receipt.cost, CostUnits(PRICES.get + PRICES.patch));
    assert_eq!(
        removed.receipt.details,
        details(Some(&before), Some(&after))
    );
    assert_eq!(removed.receipt.details.cells_deleted, 2);
    assert_eq!(removed.receipt.details.index_leaves, 1);
}

#[test]
fn minting_an_existing_account_preserves_balance_and_transaction_nonce() {
    let mut view = view();
    view.fetch_add_balance(ALICE, UserBalance::from_u64(100))
        .unwrap();
    view.fetch_increment_acc_nonce(ALICE).unwrap();
    let read = view
        .get_entity_creation_nonce(ALICE, ReadMode::ViewWithOverlay)
        .unwrap();
    assert_eq!(read.value, EntityCreationNonce::new(0));
    assert_eq!(read.receipt.cost, CostUnits(PRICES.get));
    let incremented = view.fetch_increment_entity_creation_nonce(ALICE).unwrap();
    assert_eq!(incremented.value, EntityCreationNonce::new(0));
    assert_eq!(
        incremented.receipt.cost,
        CostUnits(PRICES.get + PRICES.patch)
    );
    assert_eq!(
        incremented.receipt.details,
        WriteDetails {
            cells_created: 1,
            cell_bytes_written: 24,
            ..WriteDetails::ZERO
        }
    );
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
        UserBalance::from_u64(100)
    );
    assert_eq!(
        view.get_acc_nonce(ALICE, ReadMode::ViewWithOverlay)
            .unwrap()
            .get(),
        1
    );

    let created = view
        .update_entity(EntityUpdates::create(entity(1, 10)))
        .unwrap();
    assert_eq!(created.receipt.cost, CostUnits(PRICES.get + PRICES.create));
    assert_eq!(created.receipt.details, details(None, Some(&entity(1, 10))));
}
