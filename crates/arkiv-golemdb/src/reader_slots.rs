//! Reads that wait for a free reader slot.
//!
//! MDBX has a fixed number of reader slots, and an open read transaction holds
//! one. GolemDB opens a read transaction for every call, so with every slot
//! held the next call fails. [`ReaderSlots`] hands the slots out itself: a read
//! with none free waits for one.
//!
//! No call opens a second read transaction while it holds one, so a waiting
//! read never waits for itself.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use golemdb_storage::{ReadTransaction, Result, Store, StoreStats, Table};

/// How many reader slots GolemDB's MDBX has. GolemDB sets none, so this is
/// libmdbx's own count for 4 KiB system pages; larger pages have more.
pub(crate) const MDBX_READER_SLOTS: usize = 112;

/// `store` with at most `slots` read transactions open at once. One more waits
/// until another closes.
#[derive(Clone)]
pub(crate) struct ReaderSlots<S> {
    store: S,
    free: Arc<FreeSlots>,
}

/// How many slots are free, and the signal that one came back.
struct FreeSlots {
    count: Mutex<usize>,
    returned: Condvar,
}

/// One held slot, given back when dropped.
struct Slot(Arc<FreeSlots>);

impl<S> ReaderSlots<S> {
    pub(crate) fn new(store: S, slots: usize) -> Self {
        let free = FreeSlots {
            count: Mutex::new(slots),
            returned: Condvar::new(),
        };
        Self {
            store,
            free: Arc::new(free),
        }
    }
}

impl FreeSlots {
    fn count(&self) -> MutexGuard<'_, usize> {
        self.count
            .lock()
            .expect("the slot count's lock is never poisoned")
    }

    /// Take a slot, waiting while none is free.
    fn take(self: &Arc<Self>) -> Slot {
        let mut count = self
            .returned
            .wait_while(self.count(), |count| *count == 0)
            .expect("the slot count's lock is never poisoned");
        *count -= 1;
        Slot(Arc::clone(self))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.count() += 1;
        self.0.returned.notify_one();
    }
}

/// Every method is passed on: one left to its default would hide the store's
/// own answer.
impl<S: Store> Store for ReaderSlots<S> {
    fn max_key_size(&self) -> usize {
        self.store.max_key_size()
    }

    fn max_value_size(&self) -> usize {
        self.store.max_value_size()
    }

    fn stats(&self) -> Result<Option<StoreStats>> {
        self.store.stats()
    }

    type Read<'db>
        = SlotRead<S::Read<'db>>
    where
        Self: 'db;
    type Write<'db>
        = S::Write<'db>
    where
        Self: 'db;

    fn begin_read(&self) -> Result<Self::Read<'_>> {
        let slot = self.free.take();
        Ok(SlotRead {
            read: self.store.begin_read()?,
            _slot: slot,
        })
    }

    /// A write transaction holds no reader slot.
    fn begin_write(&self) -> Result<Self::Write<'_>> {
        self.store.begin_write()
    }
}

/// A read transaction and the slot it holds.
pub(crate) struct SlotRead<R> {
    read: R,
    // Dropped after `read`, so MDBX has its slot back before this one is free.
    _slot: Slot,
}

impl<R: ReadTransaction> ReadTransaction for SlotRead<R> {
    type Cursor<'tx>
        = R::Cursor<'tx>
    where
        Self: 'tx;

    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.read.get(table, key)
    }

    fn cursor(&self, table: Table, key: &[u8]) -> Result<Self::Cursor<'_>> {
        self.read.cursor(table, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use golemdb_storage::{MemoryStore, StorageError};
    use golemdb_storage_mdbx::MdbxStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SLOTS: usize = 2;
    /// Threads that read at once, more than there are slots.
    const READERS: usize = 8;
    /// Reads per thread, enough for the threads to overlap.
    const READS: usize = 2_000;

    #[test]
    fn no_more_reads_are_open_than_there_are_slots() {
        let store = ReaderSlots::new(MemoryStore::new(), SLOTS);
        let open = AtomicUsize::new(0);
        let most_open = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..READERS {
                scope.spawn(|| {
                    for _ in 0..READS {
                        let read = store.begin_read().unwrap();
                        let now_open = open.fetch_add(1, Ordering::SeqCst) + 1;
                        most_open.fetch_max(now_open, Ordering::SeqCst);
                        open.fetch_sub(1, Ordering::SeqCst);
                        drop(read);
                    }
                });
            }
        });
        assert!(most_open.load(Ordering::SeqCst) <= SLOTS);
        assert_eq!(*store.free.count(), SLOTS, "every slot came back");
    }

    /// A store whose reads fail to open.
    #[derive(Clone)]
    struct NoReads(MemoryStore);

    impl Store for NoReads {
        type Read<'db> = <MemoryStore as Store>::Read<'db>;
        type Write<'db> = <MemoryStore as Store>::Write<'db>;

        fn begin_read(&self) -> Result<Self::Read<'_>> {
            Err(StorageError::Full)
        }

        fn begin_write(&self) -> Result<Self::Write<'_>> {
            self.0.begin_write()
        }
    }

    #[test]
    fn a_read_that_fails_to_open_holds_no_slot() {
        let store = ReaderSlots::new(NoReads(MemoryStore::new()), SLOTS);
        assert!(store.begin_read().is_err());
        assert_eq!(*store.free.count(), SLOTS);
    }

    /// MDBX refuses a read it has no slot for, so every read here opens only
    /// if the store has as many slots as this hands out.
    #[test]
    fn mdbx_has_a_slot_for_every_one_handed_out() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ReaderSlots::new(MdbxStore::open(dir.path()).unwrap(), MDBX_READER_SLOTS);
        let reads: Vec<_> = (0..MDBX_READER_SLOTS).map(|_| store.begin_read()).collect();
        assert!(reads.iter().all(Result::is_ok));
    }

    #[test]
    fn the_stores_own_limits_and_stats_are_passed_on() {
        let dir = tempfile::TempDir::new().unwrap();
        let mdbx = MdbxStore::open(dir.path()).unwrap();
        let store = ReaderSlots::new(mdbx.clone(), SLOTS);
        assert_eq!(store.max_key_size(), mdbx.max_key_size());
        assert_eq!(store.max_value_size(), mdbx.max_value_size());
        assert_eq!(store.stats().unwrap(), mdbx.stats().unwrap());
        // MDBX has limits and stats of its own, unlike the defaults.
        assert_ne!(store.max_key_size(), usize::MAX);
        assert!(store.stats().unwrap().is_some());
    }
}
