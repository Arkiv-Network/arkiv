//! `ValkeyStore` against the shared store contract.
//!
//! These need a real server, so they are `#[ignore]` by default and run in CI
//! against a Valkey service container:
//!
//! ```sh
//! VALKEY_URL=redis://127.0.0.1:6379 cargo test -p arkiv-valkey -- --ignored
//! ```
//!
//! Gating on a live server rather than a fake is deliberate. A fake would only
//! prove that our model of Valkey is self-consistent — which
//! [`MemStore`](arkiv_interfaces::store::reference::MemStore) already does,
//! more cheaply and without a network. The whole point of this crate is the
//! behaviour of the real thing.

use arkiv_interfaces::store::conformance;
use arkiv_valkey::ValkeyStore;

/// Where the server is, or a clear explanation of why we cannot proceed.
///
/// Panics rather than silently passing: a green test that never connected is
/// worse than no test.
fn url() -> String {
    std::env::var("VALKEY_URL").expect(
        "these tests need a Valkey server: set VALKEY_URL \
         (for example redis://127.0.0.1:6379)",
    )
}

/// A store on a namespace nothing else is using, so assertions that each build
/// a fresh store cannot see one another's writes.
fn new_store() -> ValkeyStore {
    ValkeyStore::ephemeral(&url()).expect("connect to the valkey server")
}

#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn store_conformance() {
    conformance::run_all(&new_store);
}

#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn store_ext_conformance() {
    conformance::run_all_ext(&new_store);
}

/// Committed state outlives the connection that wrote it, and reconnecting to
/// the same namespace resumes from it — the property that separates this from
/// an in-memory store.
#[test]
#[ignore = "needs a valkey server; set VALKEY_URL and run with --ignored"]
fn committed_state_survives_reconnection() {
    use arkiv_interfaces::store::{Cell, ReadTarget, Store, TypeId};

    let namespace = format!("arkiv-test:reconnect:{}", std::process::id());
    let key = arkiv_interfaces::store::RecordKey([7u8; 32]);

    let committed = {
        let mut store = ValkeyStore::connect(&url(), &namespace).expect("connect");
        let branch = store.begin(None).expect("begin");
        store
            .create(
                branch,
                key,
                vec![("n".to_owned(), Cell::attribute(TypeId::U64, vec![0; 8]))],
                None,
            )
            .expect("create");
        store.commit(branch).expect("commit")
    };

    let reopened = ValkeyStore::connect(&url(), &namespace).expect("reconnect");
    assert_eq!(
        reopened.head(),
        committed,
        "the head survived the reconnect"
    );
    assert!(
        reopened
            .get(ReadTarget::Commit(committed), key, None, None)
            .expect("get")
            .into_value()
            .is_some(),
        "and so did the record"
    );
    reopened.drop_namespace().expect("clean up");
}
