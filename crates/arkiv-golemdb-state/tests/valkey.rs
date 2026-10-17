//! [`GolemStateView`] over a real, out-of-process store.
//!
//! Every other test in this crate runs against `MemStore`, which shares the
//! process and cannot disagree with our model of a store. These run against
//! Valkey, so they are `#[ignore]` by default:
//!
//! ```sh
//! VALKEY_URL=redis://127.0.0.1:6379 cargo test -p arkiv-golemdb-state -- --ignored
//! ```
//!
//! What they are for is the seam, not the logic: that a committed entity is
//! queryable through the store's own index, that the digest survives a
//! round-trip, and that account records land in the reserved key namespace.

use arkiv_golemdb_state::GolemStateView;
use arkiv_interfaces::entity::{Attribute, AttributeValue, CreationFlags, Entity};
use arkiv_interfaces::primitives::{UserAddress, UserBalance};
use arkiv_interfaces::statemanager::{
    AccountBalancesStore, BlockRef, EntityStore, EntityUpdates, EqualityIndexStore,
    RangeIndexStore, ReadMode, SessionId,
};
use arkiv_interfaces::store::Store;
use arkiv_valkey::ValkeyStore;

const ALICE: UserAddress = [0xaa; 20];

fn url() -> String {
    std::env::var("VALKEY_URL").expect(
        "these tests need a Valkey server: set VALKEY_URL \
         (for example redis://127.0.0.1:6379)",
    )
}

/// A view on a namespace nothing else is using.
fn view() -> GolemStateView<ValkeyStore> {
    let store = ValkeyStore::ephemeral(&url()).expect("connect to the valkey server");
    let origin = store.head();
    let branch = store.begin(Some(origin)).expect("begin");
    GolemStateView::new(
        store,
        branch,
        origin,
        BlockRef::new(10, [0xBB; 32]),
        SessionId([1; 16]),
    )
}

fn entity(key: u8, level: i32) -> Entity {
    Entity {
        key: [key; 32],
        owner: ALICE,
        creator: ALICE,
        created_at_block: 1,
        last_modified_at_block: 1,
        expires_at: 100,
        creation_flags: CreationFlags::NONE,
        content_type: b"text/plain".to_vec(),
        payload: vec![key],
        attributes: vec![Attribute::new(
            b"level".to_vec(),
            AttributeValue::Int(level),
        )],
    }
}

/// Reopen the same view over the commit its writes landed in.
fn commit_and_reopen(view: GolemStateView<ValkeyStore>) -> GolemStateView<ValkeyStore> {
    let branch = view.branch();
    let store = view.into_store();
    let origin = store.commit(branch).expect("commit");
    let branch = store.begin(Some(origin)).expect("begin");
    GolemStateView::new(
        store,
        branch,
        origin,
        BlockRef::new(11, [0xCC; 32]),
        SessionId([2; 16]),
    )
}

#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn an_entity_round_trips_through_the_network_seam() {
    let mut view = view();
    view.update_entity(EntityUpdates::create(entity(1, 10)))
        .expect("create");

    assert_eq!(
        view.get_entity([1; 32], ReadMode::ViewWithOverlay)
            .expect("overlay read"),
        Some(entity(1, 10)),
    );
    assert_eq!(
        view.get_entity([1; 32], ReadMode::ViewOnBase)
            .expect("base read"),
        None,
        "the base must not see this view's staged writes",
    );

    let view = commit_and_reopen(view);
    assert_eq!(
        view.get_entity([1; 32], ReadMode::ViewOnBase)
            .expect("read after commit"),
        Some(entity(1, 10)),
    );
}

/// The claim the whole design rests on: no index is maintained by this crate,
/// and lookups are still answered — by the store, from the cells it indexed.
#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn the_store_answers_index_lookups_nobody_built() {
    let mut view = view();
    for (key, level) in [(1u8, 10i32), (2, 20), (3, 30)] {
        view.update_entity(EntityUpdates::create(entity(key, level)))
            .expect("create");
    }
    let deltas = view.get_uncommitted_deltas().expect("deltas");
    EqualityIndexStore::apply_deltas(&mut view, &deltas).expect("equality deltas");
    RangeIndexStore::apply_deltas(&mut view, &deltas).expect("range deltas");

    let view = commit_and_reopen(view);

    assert_eq!(
        view.get_equal_entities(b"level", &AttributeValue::Int(20), ReadMode::ViewOnBase)
            .expect("equality"),
        vec![[2; 32]],
    );
    assert_eq!(
        view.get_within_range(
            b"level",
            core::ops::Bound::Included(&AttributeValue::Int(20)),
            core::ops::Bound::Unbounded,
            ReadMode::ViewOnBase,
        )
        .expect("range"),
        vec![[2; 32], [3; 32]],
    );
}

#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn accounts_and_entities_share_one_store_without_colliding() {
    let mut view = view();
    view.update_entity(EntityUpdates::create(entity(1, 10)))
        .expect("create");
    AccountBalancesStore::set_balance(&mut view, ALICE, UserBalance::from_u64(500))
        .expect("set balance");

    let view = commit_and_reopen(view);
    assert_eq!(
        view.get_balance(ALICE, ReadMode::ViewOnBase).expect("read"),
        UserBalance::from_u64(500),
    );
    assert_eq!(
        view.get_entity([1; 32], ReadMode::ViewOnBase)
            .expect("read"),
        Some(entity(1, 10)),
    );
}

/// The digest is the state root, so two nodes writing the same content must
/// agree on it — including across separate connections and namespaces.
#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn the_same_content_digests_the_same_on_two_connections() {
    let digest_of = || {
        let mut view = view();
        view.update_entity(EntityUpdates::create(entity(1, 10)))
            .expect("create");
        EntityStore::commit_store(&mut view).expect("commit")
    };
    assert_eq!(digest_of(), digest_of());
}
