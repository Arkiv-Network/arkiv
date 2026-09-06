//! An external sort for trie leaves, so a state root can be built over more
//! accounts (or storage slots) than fit in memory.
//!
//! A Merkle-Patricia root wants its leaves in hashed-key order, and a seed
//! produces them in seed order. The [`Sorter`] takes `(hashed key, leaf)` pairs
//! as they come, keeps at most [`Sorter::capacity`] bytes of them in memory,
//! and spills sorted runs to files; [`Sorter::into_sorted`] merges the runs
//! back in key order. Memory is bounded by the capacity, disk by the data.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

use alloy_primitives::B256;

/// Sorts `(B256 key, bytes)` pairs by key, spilling to `dir` past `capacity`
/// bytes of buffered pairs.
#[derive(Debug)]
pub struct Sorter {
    dir: PathBuf,
    capacity: usize,
    buffer: Vec<(B256, Vec<u8>)>,
    buffered_bytes: usize,
    runs: Vec<PathBuf>,
}

impl Sorter {
    /// A sorter spilling into `dir` (created if missing) once `capacity` bytes
    /// of pairs are buffered.
    pub fn new(dir: PathBuf, capacity: usize) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            capacity,
            buffer: Vec::new(),
            buffered_bytes: 0,
            runs: Vec::new(),
        })
    }

    /// Add one pair.
    pub fn push(&mut self, key: B256, value: Vec<u8>) -> io::Result<()> {
        self.buffered_bytes += 32 + value.len();
        self.buffer.push((key, value));
        if self.buffered_bytes >= self.capacity {
            self.spill()?;
        }
        Ok(())
    }

    /// Sort the buffer and write it out as one run: `key ‖ len(u32 BE) ‖ value`
    /// per pair.
    fn spill(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.sort_unstable_by_key(|(key, _)| *key);
        let path = self.dir.join(format!("run-{:06}", self.runs.len()));
        let mut out = BufWriter::new(File::create(&path)?);
        for (key, value) in self.buffer.drain(..) {
            out.write_all(key.as_slice())?;
            out.write_all(&(value.len() as u32).to_be_bytes())?;
            out.write_all(&value)?;
        }
        out.flush()?;
        self.runs.push(path);
        self.buffered_bytes = 0;
        Ok(())
    }

    /// Everything pushed, in ascending key order. Runs are merged as they are
    /// read, so only one buffered pair per run is in memory at a time.
    pub fn into_sorted(mut self) -> io::Result<Sorted> {
        // A small sorter never touches disk: sort in place and hand it over.
        if self.runs.is_empty() {
            self.buffer.sort_unstable_by_key(|(key, _)| *key);
            let mut pairs = std::mem::take(&mut self.buffer);
            pairs.reverse(); // so `pop` yields ascending
            return Ok(Sorted {
                memory: pairs,
                runs: Vec::new(),
                heap: BinaryHeap::new(),
                dir: self.dir.clone(),
                remove_dir: false,
            });
        }
        self.spill()?;
        let mut runs = Vec::with_capacity(self.runs.len());
        let mut heap = BinaryHeap::with_capacity(self.runs.len());
        for (index, path) in self.runs.iter().enumerate() {
            let mut run = RunReader {
                reader: BufReader::with_capacity(1 << 16, File::open(path)?),
                path: path.clone(),
            };
            if let Some((key, value)) = run.next_pair()? {
                heap.push(Reverse(Head { key, index, value }));
            }
            runs.push(run);
        }
        Ok(Sorted {
            memory: Vec::new(),
            runs,
            heap,
            dir: self.dir.clone(),
            remove_dir: true,
        })
    }
}

/// One spilled run being read back.
#[derive(Debug)]
struct RunReader {
    reader: BufReader<File>,
    path: PathBuf,
}

impl RunReader {
    fn next_pair(&mut self) -> io::Result<Option<(B256, Vec<u8>)>> {
        let mut key = [0u8; 32];
        match self.reader.read_exact(&mut key) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let mut len = [0u8; 4];
        self.reader.read_exact(&mut len)?;
        let mut value = vec![0u8; u32::from_be_bytes(len) as usize];
        self.reader.read_exact(&mut value)?;
        Ok(Some((B256::from(key), value)))
    }
}

/// The head pair of one run, ordered by key (then run index, for a stable
/// merge).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Head {
    key: B256,
    index: usize,
    value: Vec<u8>,
}

/// The merged, ascending output of a [`Sorter`]. Yields `io::Result` pairs.
#[derive(Debug)]
pub struct Sorted {
    memory: Vec<(B256, Vec<u8>)>,
    runs: Vec<RunReader>,
    heap: BinaryHeap<Reverse<Head>>,
    dir: PathBuf,
    remove_dir: bool,
}

impl Iterator for Sorted {
    type Item = io::Result<(B256, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(pair) = self.memory.pop() {
            return Some(Ok(pair));
        }
        let Reverse(Head { key, index, value }) = self.heap.pop()?;
        match self.runs[index].next_pair() {
            Ok(Some((next_key, next_value))) => self.heap.push(Reverse(Head {
                key: next_key,
                index,
                value: next_value,
            })),
            Ok(None) => {}
            Err(e) => return Some(Err(e)),
        }
        Some(Ok((key, value)))
    }
}

impl Drop for Sorted {
    fn drop(&mut self) {
        for run in &self.runs {
            let _ = std::fs::remove_file(&run.path);
        }
        if self.remove_dir {
            let _ = std::fs::remove_dir(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed(n: u64) -> (B256, Vec<u8>) {
        let mut key = [0u8; 32];
        key[24..].copy_from_slice(&n.to_be_bytes());
        (B256::from(key), n.to_le_bytes().to_vec())
    }

    fn collect(sorted: Sorted) -> Vec<(B256, Vec<u8>)> {
        sorted.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn small_input_sorts_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = Sorter::new(dir.path().join("s"), 1 << 20).unwrap();
        for n in [5u64, 3, 9, 1] {
            let (k, v) = keyed(n);
            sorter.push(k, v).unwrap();
        }
        let out = collect(sorter.into_sorted().unwrap());
        let keys: Vec<u64> = out
            .iter()
            .map(|(_, v)| u64::from_le_bytes(v[..].try_into().unwrap()))
            .collect();
        assert_eq!(keys, [1, 3, 5, 9]);
        assert!(!dir.path().join("s").join("run-000000").exists());
    }

    #[test]
    fn large_input_merges_spilled_runs_in_order() {
        let dir = tempfile::tempdir().unwrap();
        // A capacity of a few pairs forces many runs.
        let mut sorter = Sorter::new(dir.path().join("s"), 200).unwrap();
        let mut ns: Vec<u64> = (0..1000).map(|i| (i * 7919) % 1000).collect();
        ns.dedup();
        for &n in &ns {
            let (k, v) = keyed(n);
            sorter.push(k, v).unwrap();
        }
        assert!(sorter.runs.len() > 10);
        let out = collect(sorter.into_sorted().unwrap());
        let mut expected = ns.clone();
        expected.sort_unstable();
        let got: Vec<u64> = out
            .iter()
            .map(|(_, v)| u64::from_le_bytes(v[..].try_into().unwrap()))
            .collect();
        assert_eq!(got, expected);
        assert!(!dir.path().join("s").exists(), "runs and dir cleaned up");
    }

    #[test]
    fn empty_sorter_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sorter = Sorter::new(dir.path().join("s"), 16).unwrap();
        assert!(collect(sorter.into_sorted().unwrap()).is_empty());
    }
}
