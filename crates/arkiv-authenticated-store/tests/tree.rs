use alloy_primitives::B256;
use arkiv_authenticated_store::{EMPTY_ROOT, MdbxStore, MemoryStore, RecordStore, Tree};
use eyre::Result;
use std::{
    collections::BTreeMap,
    ops::{Bound, ControlFlow},
};

fn bytes(n: u64) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

#[test]
fn canonical_roots_and_real_deletion() -> Result<()> {
    let store = MemoryStore::default();
    let mut a = Tree::open(store.clone(), EMPTY_ROOT)?;
    let mut b = Tree::open(store, EMPTY_ROOT)?;
    for i in 0..200 {
        a.insert(bytes(i), bytes(i * 7))?;
    }
    for i in (0..200).rev() {
        b.insert(bytes(i), bytes(i * 7))?;
    }
    assert_eq!(
        a.root(),
        b.root(),
        "shape and root must not depend on insertion order"
    );
    let before = a.root();
    a.insert(bytes(400), bytes(123))?;
    a.remove(&bytes(400))?;
    assert_eq!(a.root(), before, "deletion removes all structural traces");
    a.insert(bytes(5), bytes(888))?;
    assert_ne!(a.root(), before);
    a.insert(bytes(5), bytes(35))?;
    assert_eq!(a.root(), before);
    for i in 0..200 {
        a.remove(&bytes(i))?;
    }
    assert_eq!(a.root(), EMPTY_ROOT);
    Ok(())
}

#[test]
fn mixed_operations_match_an_ordered_map() -> Result<()> {
    let mut tree = Tree::open(MemoryStore::default(), EMPTY_ROOT)?;
    let mut reference = BTreeMap::new();
    let mut random = 123456789u64;
    for step in 0..1500 {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        let k = bytes((random >> 32) % 120);
        if random.is_multiple_of(4) {
            tree.remove(&k)?;
            reference.remove(&k);
        } else {
            tree.insert(k.clone(), bytes(step))?;
            reference.insert(k.clone(), bytes(step));
        }
        assert_eq!(tree.get(&k)?, reference.get(&k).cloned());
        if step % 100 == 0 {
            let mut found = BTreeMap::new();
            tree.scan(Bound::Unbounded, Bound::Unbounded, |k, v| {
                found.insert(k.to_vec(), v.to_vec());
                Ok(ControlFlow::Continue(()))
            })?;
            assert_eq!(found, reference);
            let mut rebuilt = Tree::open(MemoryStore::default(), EMPTY_ROOT)?;
            for (k, v) in &reference {
                rebuilt.insert(k.clone(), v.clone())?;
            }
            assert_eq!(tree.root(), rebuilt.root());
        }
    }
    Ok(())
}

#[test]
fn bounded_scan_and_early_stop_do_not_read_the_whole_tree() -> Result<()> {
    let mut tree = Tree::open(MemoryStore::default(), EMPTY_ROOT)?;
    for i in 0..10_000 {
        tree.insert(bytes(i), bytes(i))?;
    }
    let mut found = Vec::new();
    let stats = tree.scan(
        Bound::Included(&bytes(5000)),
        Bound::Excluded(&bytes(5010)),
        |k, _| {
            found.push(k.to_vec());
            Ok(ControlFlow::Continue(()))
        },
    )?;
    assert_eq!(found, (5000..5010).map(bytes).collect::<Vec<_>>());
    assert_eq!(stats.values_read, 10);
    assert!(stats.nodes_read < 100, "{stats:?}");
    let stats = tree.scan(Bound::Unbounded, Bound::Excluded(&bytes(10)), |_, _| {
        Ok(ControlFlow::Continue(()))
    })?;
    assert_eq!(stats.values_read, 10);
    assert!(
        stats.nodes_read < 100,
        "upper bound must prune the suffix: {stats:?}"
    );
    let stats = tree.scan(Bound::Unbounded, Bound::Unbounded, |_, _| {
        Ok(ControlFlow::Break(()))
    })?;
    assert_eq!(stats.values_read, 1);
    assert!(stats.nodes_read < 100);
    let stats = tree.scan(
        Bound::Included(&bytes(10)),
        Bound::Excluded(&bytes(10)),
        |_, _| panic!("empty range"),
    )?;
    assert_eq!(stats.values_read, 0);
    tree.scan(
        Bound::Included(&bytes(10)),
        Bound::Included(&bytes(5)),
        |_, _| panic!("reversed range"),
    )?;
    Ok(())
}

#[test]
fn persisted_snapshots_survive_reopen_and_divergent_forks() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = MdbxStore::open(dir.path())?;
    let mut original = Tree::open(store.clone(), EMPTY_ROOT)?;
    original.insert(bytes(1), b"parent".to_vec())?;
    let parent = original.persist()?;
    let mut fork = Tree::open(store.clone(), parent)?;
    original.insert(bytes(1), b"left".to_vec())?;
    let left = original.persist()?;
    fork.insert(bytes(1), b"right".to_vec())?;
    let right = fork.persist()?;
    drop(original);
    drop(fork);
    drop(store);
    let reopened = MdbxStore::open(dir.path())?;
    for (root, value) in [(parent, "parent"), (left, "left"), (right, "right")] {
        assert_eq!(
            Tree::open(reopened.clone(), root)?.get(&bytes(1))?,
            Some(value.as_bytes().to_vec())
        );
    }
    assert!(Tree::open(reopened, B256::repeat_byte(123)).is_err());
    Ok(())
}

#[test]
fn failed_batch_keeps_parent_root() -> Result<()> {
    let mut tree = Tree::open(MemoryStore::default(), EMPTY_ROOT)?;
    tree.insert(bytes(1), bytes(5))?;
    let parent = tree.root();
    let result: Result<()> = tree.transaction(|tree| {
        tree.insert(bytes(1), bytes(6))?;
        tree.insert(bytes(2), bytes(7))?;
        eyre::bail!("revert");
    });
    assert!(result.is_err());
    assert_eq!(tree.root(), parent);
    assert_eq!(tree.get(&bytes(1))?, Some(bytes(5)));
    assert_eq!(tree.get(&bytes(2))?, None);
    let result = tree.transaction(|tree| {
        tree.remove(&bytes(1))?;
        tree.persist()
    });
    assert!(
        result.is_err(),
        "cannot flush away an unpersisted rollback root"
    );
    assert_eq!(tree.root(), parent);
    assert_eq!(tree.get(&bytes(1))?, Some(bytes(5)));
    tree.persist()?;
    Ok(())
}

#[derive(Clone)]
struct Corrupt(MemoryStore);
impl RecordStore for Corrupt {
    fn get(&self, hash: B256) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(hash)?.map(|mut b| {
            b[0] ^= 1;
            b
        }))
    }
    fn write(&self, _: &BTreeMap<B256, Vec<u8>>) -> Result<()> {
        eyre::bail!("disk failure")
    }
}

#[test]
fn corrupted_records_and_failed_persistence_are_errors() -> Result<()> {
    let store = MemoryStore::default();
    let mut tree = Tree::open(store.clone(), EMPTY_ROOT)?;
    tree.insert(bytes(1), bytes(2))?;
    let root = tree.persist()?;
    assert!(Tree::open(Corrupt(store.clone()), root).is_err());
    let mut tree = Tree::open(Corrupt(store), EMPTY_ROOT)?;
    tree.insert(bytes(1), bytes(2))?;
    assert!(tree.persist().is_err());
    assert_eq!(
        tree.get(&bytes(1))?,
        Some(bytes(2)),
        "failed flush must preserve staged writes for retry"
    );
    Ok(())
}

#[derive(Clone, Default)]
struct CountWrites {
    store: MemoryStore,
    records: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RecordStore for CountWrites {
    fn get(&self, hash: B256) -> Result<Option<Vec<u8>>> {
        self.store.get(hash)
    }
    fn write(&self, records: &BTreeMap<B256, Vec<u8>>) -> Result<()> {
        self.records
            .fetch_add(records.len(), std::sync::atomic::Ordering::Relaxed);
        self.store.write(records)
    }
}

#[test]
fn persistence_discards_intermediate_versions_and_reuses_durable_children() -> Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    let store = CountWrites::default();
    let mut tree = Tree::open(store.clone(), EMPTY_ROOT)?;
    for i in 0..100 {
        tree.insert(bytes(1), bytes(i))?;
    }
    tree.persist()?;
    assert_eq!(
        store.records.load(Relaxed),
        2,
        "only final node and value should reach disk"
    );
    for i in 2..100 {
        tree.insert(bytes(i), bytes(i))?;
    }
    let root = tree.persist()?;
    let original = Tree::open(store.clone(), root)?;
    tree.remove(&bytes(50))?;
    tree.insert(bytes(75), bytes(999))?;
    let fork = tree.persist()?;
    let fork = Tree::open(store, fork)?;
    for i in 1..100 {
        let old = bytes(if i == 1 { 99 } else { i });
        assert_eq!(original.get(&bytes(i))?, Some(old.clone()));
        let new = match i {
            50 => None,
            75 => Some(bytes(999)),
            _ => Some(old),
        };
        assert_eq!(fork.get(&bytes(i))?, new);
    }
    Ok(())
}
