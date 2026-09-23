//! Where a seed's accounts go as they are finished: into memory, for the
//! genesis and alloc JSON shapes, or straight to a `reth init-state` dump with
//! the state root built on the way, for seeds bigger than memory.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, KECCAK256_EMPTY, U256, keccak256};
use alloy_rlp::Encodable;
use alloy_trie::root::{state_root_ref_unhashed, storage_root_unhashed};
use alloy_trie::{HashBuilder, Nibbles, TrieAccount};
use eyre::{Result, WrapErr, bail};

use crate::sort::{Sorted, Sorter};

/// What a sink knows once every account is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    /// The state root over every account handed to the sink.
    pub state_root: B256,
    /// How many accounts that was.
    pub accounts: u64,
}

/// A destination for finished accounts.
///
/// The builder hands over each account once, when nothing will touch it again.
/// One account — the store's system account — is written to for the whole
/// seed while most of its storage is write-once, so its slots may arrive early
/// through [`storage_part`](Self::storage_part) and its record last through
/// [`account`](Self::account); the sink joins the two.
pub trait AccountSink {
    /// A finished account. Any storage handed earlier for the same address
    /// belongs to it too.
    fn account(&mut self, address: Address, account: GenesisAccount) -> Result<()>;

    /// One storage slot of an account whose record follows later. Zero values
    /// are not storage and are ignored.
    fn storage_part(&mut self, address: Address, slot: B256, value: B256) -> Result<()>;

    /// No more accounts: the root and the count.
    fn finish(&mut self) -> Result<Finished>;
}

/// Collects the alloc in memory — for the genesis and alloc JSON shapes, and
/// for tests, which read the seeded state back through the store.
#[derive(Debug, Default)]
pub struct MemorySink {
    alloc: BTreeMap<Address, GenesisAccount>,
    parts: HashMap<Address, BTreeMap<B256, B256>>,
}

impl MemorySink {
    /// The collected alloc.
    pub fn into_alloc(self) -> BTreeMap<Address, GenesisAccount> {
        self.alloc
    }
}

impl AccountSink for MemorySink {
    fn account(&mut self, address: Address, mut account: GenesisAccount) -> Result<()> {
        if self.alloc.contains_key(&address) {
            bail!("account {address} is written twice");
        }
        if let Some(parts) = self.parts.remove(&address) {
            let storage = account.storage.get_or_insert_with(BTreeMap::new);
            for (slot, value) in parts {
                if storage.insert(slot, value).is_some() {
                    bail!("account {address}: slot {slot} handed over twice");
                }
            }
        }
        self.alloc.insert(address, account);
        Ok(())
    }

    fn storage_part(&mut self, address: Address, slot: B256, value: B256) -> Result<()> {
        if value.is_zero() {
            return Ok(());
        }
        if self
            .parts
            .entry(address)
            .or_default()
            .insert(slot, value)
            .is_some()
        {
            bail!("account {address}: slot {slot} handed over twice");
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Finished> {
        if let Some(address) = self.parts.keys().next() {
            bail!("storage was handed over for {address}, whose record never came");
        }
        Ok(Finished {
            state_root: state_root_ref_unhashed(self.alloc.iter()),
            accounts: self.alloc.len() as u64,
        })
    }
}

/// Bytes of leaves a [`Sorter`] buffers before spilling a run.
const SORT_BUFFER: usize = 64 << 20;

/// Storage handed over early for one account: the raw slots, in arrival order,
/// for the dump line; the hashed slots, sorting, for the storage root.
#[derive(Debug)]
struct Spill {
    raw: BufWriter<File>,
    raw_path: PathBuf,
    hashed: Sorter,
    slots: u64,
}

/// Writes a `reth init-state` dump as accounts come, and builds the state root
/// alongside: each account is one line of the dump and one leaf in an
/// external sort by hashed address, merged into a hash builder at the end. The
/// root goes into the dump's first line, reserved at creation and rewritten
/// in place. Memory is bounded by the sort buffers, not by the seed.
#[derive(Debug)]
pub struct StreamSink {
    out: BufWriter<File>,
    leaves: Sorter,
    spills: HashMap<Address, Spill>,
    scratch: PathBuf,
    accounts: u64,
}

impl StreamSink {
    /// Create the dump at `path`, sorting through `scratch` (created, and
    /// removed again by [`finish`](AccountSink::finish)).
    pub fn create(path: &Path, scratch: PathBuf) -> Result<Self> {
        let file =
            File::create(path).wrap_err_with(|| format!("create dump {}", path.display()))?;
        let mut out = BufWriter::new(file);
        write_root_line(&mut out, B256::ZERO)?;
        let leaves = Sorter::new(scratch.join("accounts"), SORT_BUFFER)
            .wrap_err_with(|| format!("create sort scratch under {}", scratch.display()))?;
        Ok(Self {
            out,
            leaves,
            spills: HashMap::new(),
            scratch,
            accounts: 0,
        })
    }

    /// The account's leaf: its RLP, keyed by hashed address, tagged with the
    /// address so a duplicate can be named.
    fn add_leaf(&mut self, address: Address, account: TrieAccount) -> Result<()> {
        let mut leaf = Vec::with_capacity(20 + 128);
        leaf.extend_from_slice(address.as_slice());
        account.encode(&mut leaf);
        self.leaves
            .push(keccak256(address), leaf)
            .wrap_err("spill account leaves")?;
        self.accounts += 1;
        Ok(())
    }

    /// One dump line, written by hand so the storage can stream: the fields
    /// `reth init-state` reads (a `GenesisAccount` plus `address`), the spilled slots (if any) first, then the
    /// record's own non-zero slots. A zero slot is no slot, and never written.
    fn write_line(
        &mut self,
        address: Address,
        account: &GenesisAccount,
        spill: Option<&mut Spill>,
    ) -> Result<()> {
        let out = &mut self.out;
        out.write_all(b"{\"address\":")?;
        serde_json::to_writer(&mut *out, &address)?;
        if let Some(nonce) = account.nonce {
            write!(out, ",\"nonce\":\"{nonce:#x}\"")?;
        }
        out.write_all(b",\"balance\":")?;
        serde_json::to_writer(&mut *out, &account.balance)?;
        if let Some(code) = &account.code {
            out.write_all(b",\"code\":")?;
            serde_json::to_writer(&mut *out, code)?;
        }
        // `storage` is opened by the first slot and left out when there is
        // none, as serde would leave out a `None`.
        let mut first = true;
        if let Some(spill) = spill {
            spill.raw.flush()?;
            let mut raw = BufReader::new(File::open(&spill.raw_path)?);
            let mut pair = [0u8; 64];
            loop {
                match raw.read_exact(&mut pair) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e.into()),
                }
                out.write_all(if first { b",\"storage\":{" } else { b"," })?;
                first = false;
                serde_json::to_writer(&mut *out, &B256::from_slice(&pair[..32]))?;
                out.write_all(b":")?;
                serde_json::to_writer(&mut *out, &B256::from_slice(&pair[32..]))?;
            }
        }
        for (slot, value) in account.storage.iter().flatten() {
            if value.is_zero() {
                continue;
            }
            out.write_all(if first { b",\"storage\":{" } else { b"," })?;
            first = false;
            serde_json::to_writer(&mut *out, slot)?;
            out.write_all(b":")?;
            serde_json::to_writer(&mut *out, value)?;
        }
        if !first {
            out.write_all(b"}")?;
        }
        out.write_all(b"}\n")?;
        Ok(())
    }
}

impl AccountSink for StreamSink {
    fn account(&mut self, address: Address, account: GenesisAccount) -> Result<()> {
        let code_hash = account
            .code
            .as_ref()
            .map(keccak256)
            .unwrap_or(KECCAK256_EMPTY);
        let storage_root = match self.spills.remove(&address) {
            Some(mut spill) => {
                self.write_line(address, &account, Some(&mut spill))?;
                let _ = std::fs::remove_file(&spill.raw_path);
                let Spill { mut hashed, .. } = spill;
                for (slot, value) in account.storage.iter().flatten() {
                    if !value.is_zero() {
                        hashed.push(keccak256(slot), rlp_word(*value))?;
                    }
                }
                root_of_sorted(hashed.into_sorted()?)
                    .wrap_err_with(|| format!("storage root of {address}"))?
            }
            None => {
                self.write_line(address, &account, None)?;
                storage_root_unhashed(
                    account
                        .storage
                        .iter()
                        .flatten()
                        .filter(|(_, value)| !value.is_zero())
                        .map(|(slot, value)| (*slot, U256::from_be_bytes(value.0))),
                )
            }
        };
        self.add_leaf(
            address,
            TrieAccount {
                nonce: account.nonce.unwrap_or(0),
                balance: account.balance,
                storage_root,
                code_hash,
            },
        )
    }

    fn storage_part(&mut self, address: Address, slot: B256, value: B256) -> Result<()> {
        if value.is_zero() {
            return Ok(());
        }
        let spill = match self.spills.get_mut(&address) {
            Some(spill) => spill,
            None => {
                let raw_path = self.scratch.join(format!("storage-{address}"));
                let raw = BufWriter::new(File::create(&raw_path)?);
                let hashed = Sorter::new(
                    self.scratch.join(format!("storage-{address}-sorted")),
                    SORT_BUFFER,
                )?;
                self.spills.entry(address).or_insert(Spill {
                    raw,
                    raw_path,
                    hashed,
                    slots: 0,
                })
            }
        };
        spill.raw.write_all(slot.as_slice())?;
        spill.raw.write_all(value.as_slice())?;
        spill.hashed.push(keccak256(slot), rlp_word(value))?;
        spill.slots += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<Finished> {
        if let Some(address) = self.spills.keys().next() {
            bail!("storage was handed over for {address}, whose record never came");
        }
        let leaves = std::mem::replace(
            &mut self.leaves,
            Sorter::new(self.scratch.join("accounts-done"), SORT_BUFFER)?,
        );
        let mut builder = HashBuilder::default();
        let mut previous: Option<(B256, Address)> = None;
        for pair in leaves.into_sorted()? {
            let (hashed, leaf) = pair?;
            let address = Address::from_slice(&leaf[..20]);
            if let Some((last, other)) = previous
                && last == hashed
            {
                bail!("account {address} is written twice (also as {other})");
            }
            builder.add_leaf(Nibbles::unpack(hashed), &leaf[20..]);
            previous = Some((hashed, address));
        }
        let state_root = builder.root();

        self.out.flush()?;
        let file = self.out.get_mut();
        file.seek(SeekFrom::Start(0))?;
        write_root_line(file, state_root)?;
        file.sync_all()?;
        let _ = std::fs::remove_dir_all(&self.scratch);
        Ok(Finished {
            state_root,
            accounts: self.accounts,
        })
    }
}

/// The dump's first line: `{"root":"0x…"}`. Fixed width, so the placeholder
/// written at creation and the root written at the end are the same size.
fn write_root_line(out: &mut impl Write, root: B256) -> io::Result<()> {
    serde_json::to_writer(&mut *out, &serde_json::json!({ "root": root }))?;
    out.write_all(b"\n")
}

/// A storage value as a trie leaf: the RLP of the word without leading zeros.
fn rlp_word(value: B256) -> Vec<u8> {
    alloy_rlp::encode_fixed_size(&U256::from_be_bytes(value.0)).to_vec()
}

/// The root over hashed-key-sorted leaves; a repeated key is an error.
fn root_of_sorted(sorted: Sorted) -> Result<B256> {
    let mut builder = HashBuilder::default();
    let mut previous = None;
    for pair in sorted {
        let (hashed, leaf) = pair?;
        if previous == Some(hashed) {
            bail!("hashed key {hashed} appears twice");
        }
        builder.add_leaf(Nibbles::unpack(hashed), &leaf);
        previous = Some(hashed);
    }
    Ok(builder.root())
}

/// Reading a dump back, for tests: the root line, then the accounts, the way
/// reth reads them.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use serde::Deserialize;
    use std::io::BufRead;

    #[derive(Deserialize)]
    struct ReadLine {
        #[serde(flatten)]
        account: GenesisAccount,
        address: Address,
    }

    pub(crate) fn read_dump(path: &Path) -> (B256, BTreeMap<Address, GenesisAccount>) {
        let reader = BufReader::new(File::open(path).unwrap());
        let mut lines = reader.lines();
        let first: serde_json::Value =
            serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        let root: B256 = serde_json::from_value(first["root"].clone()).unwrap();
        let mut alloc = BTreeMap::new();
        for line in lines {
            let line = line.unwrap();
            let ReadLine { account, address } = serde_json::from_str(&line).unwrap();
            assert!(alloc.insert(address, account).is_none(), "{address} twice");
        }
        (root, alloc)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::read_dump;
    use super::*;
    use alloy_primitives::Bytes;

    fn sample(byte: u8, slots: &[(u8, u8)]) -> (Address, GenesisAccount) {
        let storage: BTreeMap<B256, B256> = slots
            .iter()
            .map(|(k, v)| (B256::repeat_byte(*k), B256::repeat_byte(*v)))
            .collect();
        (
            Address::repeat_byte(byte),
            GenesisAccount {
                nonce: Some(byte as u64),
                balance: U256::from(byte as u64 * 1000),
                code: byte.is_multiple_of(2).then(|| Bytes::from(vec![byte; 40])),
                storage: (!storage.is_empty()).then_some(storage),
                private_key: None,
            },
        )
    }

    fn feed(sink: &mut impl AccountSink) {
        // Storage for the "system" account arrives before its record, in two
        // parts, one of them a zero that must vanish.
        let system = Address::repeat_byte(0x44);
        sink.storage_part(system, B256::repeat_byte(1), B256::repeat_byte(0xA1))
            .unwrap();
        sink.storage_part(system, B256::repeat_byte(2), B256::ZERO)
            .unwrap();
        for byte in [3u8, 1, 2] {
            let (address, account) = sample(byte, &[(byte, byte + 1)]);
            sink.account(address, account).unwrap();
        }
        sink.storage_part(system, B256::repeat_byte(3), B256::repeat_byte(0xA3))
            .unwrap();
        let (_, mut record) = sample(0x44, &[(9, 9)]);
        record.code = None;
        sink.account(system, record).unwrap();
        // An account with nothing but a balance, and one with an explicit zero
        // slot serde would have kept.
        let (address, account) = sample(5, &[]);
        sink.account(address, account).unwrap();
        let (address, mut account) = sample(6, &[(6, 6)]);
        account
            .storage
            .as_mut()
            .unwrap()
            .insert(B256::repeat_byte(7), B256::ZERO);
        sink.account(address, account).unwrap();
    }

    #[test]
    fn stream_and_memory_agree_on_root_and_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let mut memory = MemorySink::default();
        feed(&mut memory);
        let expected = memory.finish().unwrap();
        let alloc = memory.into_alloc();

        let dump = dir.path().join("state.jsonl");
        let mut stream = StreamSink::create(&dump, dir.path().join("scratch")).unwrap();
        feed(&mut stream);
        let finished = stream.finish().unwrap();
        assert_eq!(finished, expected);
        assert!(!dir.path().join("scratch").exists(), "scratch removed");

        let (root, read_back) = read_dump(&dump);
        assert_eq!(root, expected.state_root);
        // The zero slot is absent from the dump, present in memory: drop it
        // before comparing, then the two allocs are the same accounts.
        let mut alloc = alloc;
        for account in alloc.values_mut() {
            if let Some(storage) = &mut account.storage {
                storage.retain(|_, v| !v.is_zero());
            }
        }
        assert_eq!(read_back, alloc);
        assert_eq!(state_root_ref_unhashed(read_back.iter()), root);
        // The spilled account carries every slot, early and late.
        let system = &read_back[&Address::repeat_byte(0x44)];
        assert_eq!(system.storage.as_ref().unwrap().len(), 3);
    }

    #[test]
    fn a_repeated_account_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (address, account) = sample(1, &[]);
        let mut memory = MemorySink::default();
        memory.account(address, account.clone()).unwrap();
        assert!(memory.account(address, account.clone()).is_err());

        let mut stream =
            StreamSink::create(&dir.path().join("s.jsonl"), dir.path().join("scratch")).unwrap();
        stream.account(address, account.clone()).unwrap();
        stream.account(address, account).unwrap();
        let err = stream.finish().unwrap_err().to_string();
        assert!(err.contains("written twice"), "{err}");
    }

    #[test]
    fn storage_without_a_record_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut stream =
            StreamSink::create(&dir.path().join("s.jsonl"), dir.path().join("scratch")).unwrap();
        stream
            .storage_part(
                Address::repeat_byte(1),
                B256::repeat_byte(1),
                B256::repeat_byte(1),
            )
            .unwrap();
        assert!(stream.finish().is_err());
        let mut memory = MemorySink::default();
        memory
            .storage_part(
                Address::repeat_byte(1),
                B256::repeat_byte(1),
                B256::repeat_byte(1),
            )
            .unwrap();
        assert!(memory.finish().is_err());
    }

    #[test]
    fn dump_lines_are_reth_shaped() {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("s.jsonl");
        let mut stream = StreamSink::create(&dump, dir.path().join("scratch")).unwrap();
        let address = Address::repeat_byte(3);
        stream
            .account(
                address,
                GenesisAccount {
                    nonce: Some(1),
                    balance: U256::from(7u64),
                    code: Some(vec![0xFE, 0x01].into()),
                    storage: Some(
                        [(B256::repeat_byte(1), B256::repeat_byte(2))]
                            .into_iter()
                            .collect(),
                    ),
                    private_key: None,
                },
            )
            .unwrap();
        let finished = stream.finish().unwrap();
        let text = std::fs::read_to_string(&dump).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let root: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            root["root"],
            serde_json::to_value(finished.state_root).unwrap()
        );
        let line: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(line["address"], serde_json::to_value(address).unwrap());
        assert_eq!(line["nonce"], "0x1");
        assert_eq!(line["balance"], "0x7");
        assert_eq!(line["code"], "0xfe01");
        assert!(line["storage"].is_object());
    }
}
