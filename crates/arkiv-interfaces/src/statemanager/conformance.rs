//! The [`StateView`] contract, as executable assertions.
//!
//! Sibling of [`store::conformance`](crate::store::conformance), and there for
//! the same reason: Arkiv has two `StateView` implementations — the MPT host
//! and the GolemDB one — and during the migration both are live. If they
//! disagree about what a read returns, the two disagree about state, and that
//! is a fork.
//!
//! Usage, from an implementation's test module:
//!
//! ```ignore
//! #[test]
//! fn conformance() {
//!     arkiv_interfaces::statemanager::conformance::run_all(&my_view);
//! }
//! ```
//!
//! Each assertion takes a **constructor**, so every one starts from a fresh
//! view over an empty base and none can be polluted by an earlier failure.
//!
//! # What this does not assert
//!
//! - **Commitment values.** Every implementation commits to a different thing
//!   — the MPT host returns a placeholder, the GolemDB one returns the
//!   branch's content digest. Only that committing *works* is shared.
//! - **Index lookups.** They answer from committed state, and nothing here can
//!   commit: the suite holds a view, not the store behind it. Covered by each
//!   implementation's own tests instead.

use alloc::vec;
use alloc::vec::Vec;

use crate::entity::{Attribute, AttributeValue, Entity};
use crate::primitives::{EntityAddress, UserAddress, UserBalance};
use crate::statemanager::{
    BlockRef, EntityUpdates, PruningMeta, PruningStore, ReadMode, StateView,
};

const ALICE: UserAddress = [0xaa; 20];
const BOB: UserAddress = [0xbb; 20];

/// Run every [`StateView`] assertion against views built by `new_view`.
///
/// Panics on the first violation, naming the rule that broke.
pub fn run_all<V: StateView>(new_view: &dyn Fn() -> V) {
    // accounts
    an_unseen_account_is_zero(new_view);
    a_written_balance_reads_back(new_view);
    the_base_does_not_see_staged_account_writes(new_view);
    fetch_add_returns_the_balance_before(new_view);
    fetch_sub_returns_the_balance_before(new_view);
    an_overdraft_clamps_at_zero(new_view);
    an_overflow_clamps_at_the_maximum(new_view);
    compare_set_only_fires_on_a_match(new_view);
    nonces_advance_by_one_and_report_the_value_before(new_view);
    the_two_nonces_are_independent(new_view);
    accounts_do_not_bleed_into_each_other(new_view);

    // entities
    a_created_entity_reads_back_through_the_overlay(new_view);
    the_base_does_not_see_staged_entity_writes(new_view);
    a_deleted_entity_reads_as_absent(new_view);
    a_dropped_attribute_leaves_the_entity(new_view);
    two_writes_to_one_entity_collapse_into_one_delta(new_view);
    deltas_come_back_in_ascending_entity_order(new_view);

    // pruning
    the_pruning_set_stages_then_commits(new_view);
    pruning_order_is_urgent_first(new_view);

    // the view as a whole
    a_committed_view_graduates(new_view);
    a_dirty_view_does_not_graduate(new_view);
    graduating_needs_a_block_that_extends_the_base(new_view);
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn entity(key: u8) -> Entity {
    Entity {
        key: [key; 32],
        owner: ALICE,
        creator: ALICE,
        created_at_block: 1,
        last_modified_at_block: 1,
        expires_at: 100,
        content_type: b"text/plain".to_vec(),
        payload: vec![key],
        attributes: vec![Attribute::new(
            b"level".to_vec(),
            AttributeValue::Int(i32::from(key)),
        )],
        ..Entity::default()
    }
}

fn address(key: u8) -> EntityAddress {
    [key; 32]
}

fn gbp(n: u64) -> UserBalance {
    UserBalance::from_u64(n)
}

// ---------------------------------------------------------------------------
// accounts
// ---------------------------------------------------------------------------

fn an_unseen_account_is_zero<V: StateView>(new_view: &dyn Fn() -> V) {
    let view = new_view();
    for read in [ReadMode::ViewOnBase, ReadMode::ViewWithOverlay] {
        assert_eq!(
            view.get_balance(ALICE, read).unwrap(),
            gbp(0),
            "an account nothing ever touched has a zero balance"
        );
        assert_eq!(
            view.get_acc_nonce(ALICE, read).unwrap().get(),
            0,
            "and a zero transaction nonce"
        );
        assert_eq!(
            view.get_entity_creation_nonce(ALICE, read).unwrap().get(),
            0,
            "and a zero creation nonce"
        );
    }
}

fn a_written_balance_reads_back<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.set_balance(ALICE, gbp(500)).unwrap();
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
        gbp(500)
    );
}

fn the_base_does_not_see_staged_account_writes<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.set_balance(ALICE, gbp(500)).unwrap();
    view.fetch_increment_acc_nonce(ALICE).unwrap();
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewOnBase).unwrap(),
        gbp(0),
        "ViewOnBase is the state this view started from"
    );
    assert_eq!(
        view.get_acc_nonce(ALICE, ReadMode::ViewOnBase)
            .unwrap()
            .get(),
        0
    );
}

fn fetch_add_returns_the_balance_before<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.set_balance(ALICE, gbp(100)).unwrap();
    assert_eq!(
        view.fetch_add_balance(ALICE, gbp(5)).unwrap(),
        gbp(100),
        "fetch_add reports the value before the addition"
    );
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
        gbp(105)
    );
}

fn fetch_sub_returns_the_balance_before<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.set_balance(ALICE, gbp(100)).unwrap();
    assert_eq!(
        view.fetch_sub_balance(ALICE, gbp(5)).unwrap(),
        gbp(100),
        "fetch_sub reports the value before the subtraction"
    );
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
        gbp(95)
    );
}

/// Clamped, never refused. Solvency is decided upstream, and `eth_call` and
/// `eth_estimateGas` deliberately skip that check — debiting an unfunded
/// sender has to leave them at zero rather than fail the call.
fn an_overdraft_clamps_at_zero<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.set_balance(ALICE, gbp(10)).unwrap();
    assert_eq!(
        view.fetch_sub_balance(ALICE, gbp(11)).unwrap(),
        gbp(10),
        "still reports the value before"
    );
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
        gbp(0),
        "an overdraft clamps at zero rather than erroring"
    );
}

fn an_overflow_clamps_at_the_maximum<V: StateView>(new_view: &dyn Fn() -> V) {
    let max = UserBalance::from_be_bytes([0xff; 32]);
    let mut view = new_view();
    view.set_balance(ALICE, max).unwrap();
    view.fetch_add_balance(ALICE, gbp(1)).unwrap();
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
        max,
        "an overflow clamps at the maximum rather than erroring"
    );
}

fn compare_set_only_fires_on_a_match<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.set_balance(ALICE, gbp(10)).unwrap();
    assert!(
        !view.compare_set_balance(ALICE, gbp(99), gbp(1)).unwrap(),
        "a mismatched expectation reports false"
    );
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
        gbp(10),
        "and writes nothing"
    );
    assert!(view.compare_set_balance(ALICE, gbp(10), gbp(1)).unwrap());
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewWithOverlay).unwrap(),
        gbp(1)
    );
}

fn nonces_advance_by_one_and_report_the_value_before<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    assert_eq!(view.fetch_increment_acc_nonce(ALICE).unwrap().get(), 0);
    assert_eq!(view.fetch_increment_acc_nonce(ALICE).unwrap().get(), 1);
    assert_eq!(
        view.get_acc_nonce(ALICE, ReadMode::ViewWithOverlay)
            .unwrap()
            .get(),
        2
    );

    assert_eq!(
        view.fetch_increment_entity_creation_nonce(ALICE)
            .unwrap()
            .get(),
        0
    );
    assert_eq!(
        view.get_entity_creation_nonce(ALICE, ReadMode::ViewWithOverlay)
            .unwrap()
            .get(),
        1
    );
}

/// They both hang off one address, so an implementation that stores them
/// together has to keep them apart — a creation nonce is an input to every
/// minted entity address, and a transaction nonce is replay protection.
fn the_two_nonces_are_independent<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.fetch_increment_acc_nonce(ALICE).unwrap();
    view.fetch_increment_acc_nonce(ALICE).unwrap();
    view.fetch_increment_entity_creation_nonce(ALICE).unwrap();
    assert_eq!(
        view.get_acc_nonce(ALICE, ReadMode::ViewWithOverlay)
            .unwrap()
            .get(),
        2
    );
    assert_eq!(
        view.get_entity_creation_nonce(ALICE, ReadMode::ViewWithOverlay)
            .unwrap()
            .get(),
        1
    );
}

fn accounts_do_not_bleed_into_each_other<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.set_balance(ALICE, gbp(10)).unwrap();
    view.fetch_increment_acc_nonce(ALICE).unwrap();
    assert_eq!(
        view.get_balance(BOB, ReadMode::ViewWithOverlay).unwrap(),
        gbp(0)
    );
    assert_eq!(
        view.get_acc_nonce(BOB, ReadMode::ViewWithOverlay)
            .unwrap()
            .get(),
        0
    );
}

// ---------------------------------------------------------------------------
// entities
// ---------------------------------------------------------------------------

fn a_created_entity_reads_back_through_the_overlay<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.update_entity(EntityUpdates::create(entity(1)))
        .unwrap();
    assert_eq!(
        view.get_entity(address(1), ReadMode::ViewWithOverlay)
            .unwrap(),
        Some(entity(1)),
        "every field of a created entity survives the round trip"
    );
}

fn the_base_does_not_see_staged_entity_writes<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.update_entity(EntityUpdates::create(entity(1)))
        .unwrap();
    assert_eq!(
        view.get_entity(address(1), ReadMode::ViewOnBase).unwrap(),
        None
    );
}

fn a_deleted_entity_reads_as_absent<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.update_entity(EntityUpdates::create(entity(1)))
        .unwrap();
    view.update_entity(EntityUpdates::deletion(address(1)))
        .unwrap();
    assert_eq!(
        view.get_entity(address(1), ReadMode::ViewWithOverlay)
            .unwrap(),
        None
    );
}

/// `attributes` replaces the whole set, so clearing it must actually clear it.
/// An implementation that only adds leaves the old attribute queryable.
fn a_dropped_attribute_leaves_the_entity<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.update_entity(EntityUpdates::create(entity(1)))
        .unwrap();
    view.update_entity(EntityUpdates {
        entity: address(1),
        attributes: Some(Vec::new()),
        ..EntityUpdates::default()
    })
    .unwrap();
    let back = view
        .get_entity(address(1), ReadMode::ViewWithOverlay)
        .unwrap()
        .expect("the entity is still there");
    assert!(back.attributes.is_empty(), "the attribute is gone");
}

fn two_writes_to_one_entity_collapse_into_one_delta<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.update_entity(EntityUpdates::create(entity(1)))
        .unwrap();
    view.update_entity(EntityUpdates {
        entity: address(1),
        owner: Some(BOB),
        ..EntityUpdates::default()
    })
    .unwrap();
    let deltas = view.get_uncommitted_deltas().unwrap();
    assert_eq!(
        deltas.len(),
        1,
        "one entry per touched entity, not per write"
    );
    assert_eq!(deltas[0].owner, Some(BOB), "and the later write wins");
}

fn deltas_come_back_in_ascending_entity_order<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    for key in [3u8, 1, 2] {
        view.update_entity(EntityUpdates::create(entity(key)))
            .unwrap();
    }
    let seen: Vec<EntityAddress> = view
        .get_uncommitted_deltas()
        .unwrap()
        .iter()
        .map(|d| d.entity)
        .collect();
    assert_eq!(seen, vec![address(1), address(2), address(3)]);
}

// ---------------------------------------------------------------------------
// pruning
// ---------------------------------------------------------------------------

fn the_pruning_set_stages_then_commits<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    let meta = PruningMeta {
        priority: 1,
        introduced_at: 3,
    };
    view.add_to_pruning_set(address(1), meta).unwrap();
    assert_eq!(
        view.peek_top(4, ReadMode::ViewWithOverlay).unwrap().len(),
        1
    );
    assert!(view.peek_top(4, ReadMode::ViewOnBase).unwrap().is_empty());

    PruningStore::commit_store(&mut view).unwrap();
    assert_eq!(view.peek_top(4, ReadMode::ViewOnBase).unwrap().len(), 1);

    assert_eq!(
        view.take_top(4).unwrap().len(),
        1,
        "take returns what it took"
    );
    assert!(
        view.peek_top(4, ReadMode::ViewWithOverlay)
            .unwrap()
            .is_empty(),
        "and removes it"
    );
}

/// Node-local, but the order is consensus: higher priority first, then oldest.
fn pruning_order_is_urgent_first<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    view.add_to_pruning_set(
        address(1),
        PruningMeta {
            priority: 1,
            introduced_at: 1,
        },
    )
    .unwrap();
    view.add_to_pruning_set(
        address(2),
        PruningMeta {
            priority: 9,
            introduced_at: 50,
        },
    )
    .unwrap();
    let top = view.peek_top(2, ReadMode::ViewWithOverlay).unwrap();
    assert_eq!(
        top[0].0,
        address(2),
        "the higher priority comes first regardless of age"
    );
}

// ---------------------------------------------------------------------------
// the view as a whole
// ---------------------------------------------------------------------------

fn a_committed_view_graduates<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    let base = view.base();
    view.update_entity(EntityUpdates::create(entity(1)))
        .unwrap();
    view.set_balance(ALICE, gbp(10)).unwrap();
    StateView::commit(&mut view).unwrap();

    let next = BlockRef::new(base.height + 1, [0xCC; 32]);
    let commit = view.graduate(next).expect("a committed view graduates");
    assert_eq!(commit.block, next);
    assert_eq!(commit.parent, base);
}

fn a_dirty_view_does_not_graduate<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    let base = view.base();
    view.update_entity(EntityUpdates::create(entity(1)))
        .unwrap();
    assert!(
        view.graduate(BlockRef::new(base.height + 1, [0xCC; 32]))
            .is_err(),
        "staged writes must be committed before graduating"
    );
}

fn graduating_needs_a_block_that_extends_the_base<V: StateView>(new_view: &dyn Fn() -> V) {
    let mut view = new_view();
    let base = view.base();
    StateView::commit(&mut view).unwrap();
    assert!(
        view.graduate(BlockRef::new(base.height + 7, [0xCC; 32]))
            .is_err(),
        "a block that does not extend the base is not this view's block"
    );
}
