//! Integration tests for the query interpreter.
//!
//! Each test builds state via the public op handlers (`create`,
//! `delete`, `transfer`) against an [`InMemoryStateAdapter`], then parses
//! and evaluates a query and asserts on the resulting ID set.
//! Mirrors `arkiv-storage-service/query/evaluate_test.go` for the
//! grammar subset.

use alloy_primitives::{Address, B256, U256};
use arkiv_db_engine::query::parse;
use arkiv_db_engine::test_utils::{InMemoryStateAdapter, InMemoryStateDb};
use arkiv_db_engine::{
    ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Attribute, create, delete, resolve_id, transfer,
    update,
};

fn alice() -> Address {
    Address::repeat_byte(0xaa)
}
fn bob() -> Address {
    Address::repeat_byte(0xbb)
}
fn carol() -> Address {
    Address::repeat_byte(0xcc)
}
fn key_n(n: u8) -> B256 {
    B256::from([n; 32])
}

fn fresh() -> InMemoryStateDb {
    InMemoryStateDb::default()
}

#[track_caller]
fn ids(state: &mut InMemoryStateAdapter, q: &str) -> Vec<u64> {
    let parsed = parse(q).unwrap_or_else(|e| panic!("parse {q:?}: {e}"));
    let bm = parsed
        .evaluate(state)
        .unwrap_or_else(|e| panic!("evaluate {q:?}: {e}"));
    let mut out: Vec<u64> = bm.iter().collect();
    out.sort_unstable();
    out
}

fn create_simple(
    state: &mut InMemoryStateAdapter,
    owner: Address,
    key: B256,
    content_type: &[u8],
    expires_at: u64,
) {
    create(
        state,
        owner,
        key,
        expires_at,
        10,
        b"payload".to_vec(),
        content_type.to_vec(),
        vec![],
    )
    .expect("create");
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ── Tests ────────────────────────────────────────────────────────────

#[test]
fn star_and_dollar_all_return_every_live_entity() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    assert_eq!(ids(&mut s, "*"), vec![0, 1]);
    assert_eq!(ids(&mut s, "$all"), vec![0, 1]);
}

#[test]
fn equality_owner_address() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    let q = format!("$owner = 0x{}", hex_lower(alice().as_slice()));
    assert_eq!(ids(&mut s, &q), vec![0]);
}

#[test]
fn equality_content_type() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, alice(), key_n(2), b"text/html", 200);
    assert_eq!(ids(&mut s, r#"$contentType = "text/html""#), vec![1]);
}

#[test]
fn equality_user_string_annotation() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create(
        &mut s,
        alice(),
        key_n(1),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"tag".to_vec(),
            value_type: ATTR_STRING,
            value: b"music".to_vec(),
        }],
    )
    .expect("create");
    create(
        &mut s,
        alice(),
        key_n(2),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"tag".to_vec(),
            value_type: ATTR_STRING,
            value: b"video".to_vec(),
        }],
    )
    .expect("create");
    assert_eq!(ids(&mut s, r#"tag = "music""#), vec![0]);
}

#[test]
fn equality_user_numeric_annotation() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create(
        &mut s,
        alice(),
        key_n(1),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"score".to_vec(),
            value_type: ATTR_UINT,
            value: U256::from(42).to_be_bytes::<32>().to_vec(),
        }],
    )
    .expect("create");
    create(
        &mut s,
        alice(),
        key_n(2),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"score".to_vec(),
            value_type: ATTR_UINT,
            value: U256::from(7).to_be_bytes::<32>().to_vec(),
        }],
    )
    .expect("create");
    assert_eq!(ids(&mut s, "score = 42"), vec![0]);
}

#[test]
fn equality_user_entity_key_annotation() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    let ref_a = key_n(0xaa);
    let ref_b = key_n(0xbb);
    create(
        &mut s,
        alice(),
        key_n(1),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"ref".to_vec(),
            value_type: ATTR_ENTITY_KEY,
            value: ref_a.as_slice().to_vec(),
        }],
    )
    .expect("create");
    create(
        &mut s,
        alice(),
        key_n(2),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"ref".to_vec(),
            value_type: ATTR_ENTITY_KEY,
            value: ref_b.as_slice().to_vec(),
        }],
    )
    .expect("create");
    let q = format!("ref = 0x{}", hex_lower(ref_a.as_slice()));
    assert_eq!(ids(&mut s, &q), vec![0]);
}

#[test]
fn inclusion_user_entity_key_annotation() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    let ref_a = key_n(0xaa);
    let ref_b = key_n(0xbb);
    let ref_c = key_n(0xcc);
    for (i, r) in [(1u8, ref_a), (2, ref_b), (3, ref_c)] {
        create(
            &mut s,
            alice(),
            key_n(i),
            100,
            10,
            b"".to_vec(),
            b"text/plain".to_vec(),
            vec![Attribute {
                key: b"ref".to_vec(),
                value_type: ATTR_ENTITY_KEY,
                value: r.as_slice().to_vec(),
            }],
        )
        .expect("create");
    }
    let q = format!(
        "ref IN (0x{} 0x{})",
        hex_lower(ref_a.as_slice()),
        hex_lower(ref_c.as_slice())
    );
    assert_eq!(ids(&mut s, &q), vec![0, 2]);
}

#[test]
fn inequality_excludes_match() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    create_simple(&mut s, bob(), key_n(3), b"text/plain", 300);
    let q = format!("$owner != 0x{}", hex_lower(bob().as_slice()));
    assert_eq!(ids(&mut s, &q), vec![0]);
}

#[test]
fn and_intersects() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    create_simple(&mut s, alice(), key_n(3), b"text/html", 300);
    let q = format!(
        r#"$owner = 0x{} && $contentType = "text/plain""#,
        hex_lower(alice().as_slice())
    );
    assert_eq!(ids(&mut s, &q), vec![0]);
}

#[test]
fn or_unions() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/html", 200);
    create_simple(&mut s, carol(), key_n(3), b"text/xml", 300);
    let q = format!(
        r#"$owner = 0x{} || $owner = 0x{}"#,
        hex_lower(alice().as_slice()),
        hex_lower(bob().as_slice()),
    );
    assert_eq!(ids(&mut s, &q), vec![0, 1]);
}

#[test]
fn inclusion_unions() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    create_simple(&mut s, carol(), key_n(3), b"text/plain", 300);
    let q = format!(
        "$owner IN (0x{} 0x{})",
        hex_lower(alice().as_slice()),
        hex_lower(bob().as_slice()),
    );
    assert_eq!(ids(&mut s, &q), vec![0, 1]);
}

#[test]
fn not_inclusion_subtracts() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    create_simple(&mut s, carol(), key_n(3), b"text/plain", 300);
    let q = format!("$owner NOT IN (0x{})", hex_lower(bob().as_slice()));
    assert_eq!(ids(&mut s, &q), vec![0, 2]);
}

#[test]
fn not_around_paren_subtracts() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/html", 200);
    create_simple(&mut s, carol(), key_n(3), b"text/plain", 300);
    let q = format!(
        r#"NOT ($owner = 0x{} || $contentType = "text/html")"#,
        hex_lower(alice().as_slice()),
    );
    assert_eq!(ids(&mut s, &q), vec![2]);
}

#[test]
fn delete_removes_from_results() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, bob(), key_n(2), b"text/plain", 200);
    delete(&mut s, key_n(1)).expect("delete");
    assert_eq!(ids(&mut s, "*"), vec![1]);
}

#[test]
fn transfer_moves_owner_match() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    transfer(&mut s, key_n(1), 20, bob()).expect("transfer");

    let q_old = format!("$owner = 0x{}", hex_lower(alice().as_slice()));
    let q_new = format!("$owner = 0x{}", hex_lower(bob().as_slice()));
    assert_eq!(ids(&mut s, &q_old), Vec::<u64>::new());
    assert_eq!(ids(&mut s, &q_new), vec![0]);
}

#[test]
fn expiration_numeric_equality() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    create_simple(&mut s, alice(), key_n(2), b"text/plain", 200);
    assert_eq!(ids(&mut s, "$expiration = 100"), vec![0]);
    assert_eq!(ids(&mut s, "$expiration = 200"), vec![1]);
}

#[test]
fn resolve_id_returns_entity_rlp() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    let entity = resolve_id(&mut s, 0).expect("resolve").expect("some");
    assert_eq!(entity.owner, alice());
    assert_eq!(entity.expires_at, 100);
    assert_eq!(entity.content_type, b"text/plain".to_vec());
}

#[test]
fn resolve_id_returns_none_after_delete() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"text/plain", 100);
    delete(&mut s, key_n(1)).expect("delete");
    assert!(resolve_id(&mut s, 0).expect("resolve").is_none());
}

// ── Range and glob query tests ────────────────────────────────────────

fn create_with_price(state: &mut InMemoryStateAdapter, owner: Address, key: B256, price: u64) {
    create(
        state,
        owner,
        key,
        1000,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"price".to_vec(),
            value_type: ATTR_UINT,
            value: U256::from(price).to_be_bytes::<32>().to_vec(),
        }],
    )
    .expect("create");
}

#[test]
fn range_gt_returns_matching_entities() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_with_price(&mut s, alice(), key_n(1), 50);
    create_with_price(&mut s, alice(), key_n(2), 100);
    create_with_price(&mut s, alice(), key_n(3), 200);
    assert_eq!(ids(&mut s, "price > 100"), vec![2]);
}

#[test]
fn range_lte_returns_matching_entities() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_with_price(&mut s, alice(), key_n(1), 50);
    create_with_price(&mut s, alice(), key_n(2), 100);
    create_with_price(&mut s, alice(), key_n(3), 200);
    assert_eq!(ids(&mut s, "price <= 100"), vec![0, 1]);
}

#[test]
fn range_between_exclusive() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_with_price(&mut s, alice(), key_n(1), 50);
    create_with_price(&mut s, alice(), key_n(2), 100);
    create_with_price(&mut s, alice(), key_n(3), 200);
    create_with_price(&mut s, alice(), key_n(4), 500);
    // Entity IDs: 0=price50, 1=price100, 2=price200, 3=price500
    assert_eq!(ids(&mut s, "price > 100 && price < 500"), vec![2]);
}

#[test]
fn range_and_equality_combined() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    // id=0: image/png, price=20
    create(
        &mut s,
        alice(),
        key_n(1),
        100,
        10,
        b"".to_vec(),
        b"image/png".to_vec(),
        vec![Attribute {
            key: b"price".to_vec(),
            value_type: ATTR_UINT,
            value: U256::from(20).to_be_bytes::<32>().to_vec(),
        }],
    )
    .expect("create");
    // id=1: text/plain, price=20
    create(
        &mut s,
        alice(),
        key_n(2),
        100,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"price".to_vec(),
            value_type: ATTR_UINT,
            value: U256::from(20).to_be_bytes::<32>().to_vec(),
        }],
    )
    .expect("create");
    // id=2: image/png, price=5
    create(
        &mut s,
        alice(),
        key_n(3),
        100,
        10,
        b"".to_vec(),
        b"image/png".to_vec(),
        vec![Attribute {
            key: b"price".to_vec(),
            value_type: ATTR_UINT,
            value: U256::from(5).to_be_bytes::<32>().to_vec(),
        }],
    )
    .expect("create");
    assert_eq!(
        ids(&mut s, r#"$contentType = "image/png" && price > 10"#),
        vec![0]
    );
}

#[test]
fn glob_prefix_match() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"image/png", 100);
    create_simple(&mut s, alice(), key_n(2), b"image/jpeg", 100);
    create_simple(&mut s, alice(), key_n(3), b"text/plain", 100);
    assert_eq!(ids(&mut s, r#"$contentType ~ "image/*""#), vec![0, 1]);
}

#[test]
fn not_glob_excludes_prefix() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create_simple(&mut s, alice(), key_n(1), b"image/png", 100);
    create_simple(&mut s, alice(), key_n(2), b"image/jpeg", 100);
    create_simple(&mut s, alice(), key_n(3), b"text/plain", 100);
    assert_eq!(ids(&mut s, r#"$contentType !~ "image/*""#), vec![2]);
}

#[test]
fn update_moves_index_value() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create(
        &mut s,
        alice(),
        key_n(1),
        1000,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"price".to_vec(),
            value_type: ATTR_UINT,
            value: U256::from(100).to_be_bytes::<32>().to_vec(),
        }],
    )
    .expect("create");
    update(
        &mut s,
        key_n(1),
        20,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"price".to_vec(),
            value_type: ATTR_UINT,
            value: U256::from(200).to_be_bytes::<32>().to_vec(),
        }],
    )
    .expect("update");
    assert_eq!(ids(&mut s, "price > 150"), vec![0]);
    assert_eq!(ids(&mut s, "price < 150"), Vec::<u64>::new());
}

#[test]
fn delete_removes_index_entry() {
    let mut db = fresh();
    let mut s = InMemoryStateAdapter::new(&mut db);
    create(
        &mut s,
        alice(),
        key_n(1),
        1000,
        10,
        b"".to_vec(),
        b"text/plain".to_vec(),
        vec![Attribute {
            key: b"price".to_vec(),
            value_type: ATTR_UINT,
            value: U256::from(42).to_be_bytes::<32>().to_vec(),
        }],
    )
    .expect("create");
    assert_eq!(ids(&mut s, "price > 0"), vec![0]);
    delete(&mut s, key_n(1)).expect("delete");
    assert_eq!(ids(&mut s, "price > 0"), Vec::<u64>::new());
}

#[test]
fn range_historical_query() {
    // Simulate querying at two different block heights by using separate
    // InMemoryStateDb snapshots. block-1 has price=100; block-2 has
    // price=200 after an update.
    let key = key_n(42);
    let mut db_block1 = fresh();
    {
        let mut s = InMemoryStateAdapter::new(&mut db_block1);
        create(
            &mut s,
            alice(),
            key,
            1000,
            1,
            b"".to_vec(),
            b"text/plain".to_vec(),
            vec![Attribute {
                key: b"price".to_vec(),
                value_type: ATTR_UINT,
                value: U256::from(100).to_be_bytes::<32>().to_vec(),
            }],
        )
        .expect("create");
    }

    let mut db_block2 = db_block1.clone();
    {
        let mut s = InMemoryStateAdapter::new(&mut db_block2);
        update(
            &mut s,
            key,
            2,
            b"".to_vec(),
            b"text/plain".to_vec(),
            vec![Attribute {
                key: b"price".to_vec(),
                value_type: ATTR_UINT,
                value: U256::from(200).to_be_bytes::<32>().to_vec(),
            }],
        )
        .expect("update");
    }

    // At block 1: price=100, so price > 150 matches nothing.
    let mut s1 = InMemoryStateAdapter::new(&mut db_block1);
    assert_eq!(ids(&mut s1, "price > 150"), Vec::<u64>::new());

    // At block 2: price=200, so price > 150 matches entity 0.
    let mut s2 = InMemoryStateAdapter::new(&mut db_block2);
    assert_eq!(ids(&mut s2, "price > 150"), vec![0]);
}
