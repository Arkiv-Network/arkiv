//! Canonical home of the Arkiv state model.
//!
//! Every entity and every annotation bitmap lives in op-reth's standard
//! world-state trie as an Ethereum account:
//!
//! - **Entity account** at `entity_address(entityKey)` carries the
//!   RLP-encoded entity (payload + content type + annotations +
//!   owner/expires_at) in `code`, prefixed with `0xFE` so a stray
//!   `CALL` reverts immediately.
//! - **Pair account** at `pair_address(annot_key, annot_val)` carries a
//!   roaring64 bitmap of entity IDs as `code`. `codeHash` is
//!   `keccak256(bitmap_bytes)` by construction — every bitmap is
//!   content-addressed in the trie.
//! - **System account** (internal — see `SYSTEM_ACCOUNT_ADDRESS`) —
//!   empty-coded account that hosts the global entity counter, the
//!   per-caller `nonces` map, and the trie-committed ID ↔ address maps
//!   as storage slots. Materialised lazily on the first write via
//!   `StateAdapter::ensure_account_persists` — no genesis presence
//!   required. Separate from the precompile's registration address
//!   ([`ARKIV_ADDRESS`]) so the precompile itself stays a programmatic
//!   registration target with no on-chain dependency.
//!
//! Top-level exports:
//!
//! - Primitives: [`EntityRlp`], [`Bitmap`], address derivations,
//!   built-in annotation keys, system-account slot keys.
//! - [`StateAdapter`] trait — what the op handlers need from the
//!   underlying state (code + storage R/W). The precompile implements
//!   this over `EvmInternals`; the [`test_utils::InMemoryStateAdapter`]
//!   (behind the `test-utils` feature) implements it over an
//!   [`InMemoryStateDb`].
//! - Op handlers: [`create`], [`update`], [`extend`], [`transfer`],
//!   [`delete`], [`expire`]. All the indexing logic (system counter +
//!   ID maps, bitmap deltas across built-in and user annotations, RLP
//!   encode/decode, tombstoning) lives here. The precompile is a thin
//!   adapter: decode calldata, dispatch.

use alloy_primitives::{Address, B256, keccak256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use eyre::{Result, ensure};
use roaring::RoaringTreemap;

pub mod call;
pub mod query;
#[cfg(feature = "view")]
pub mod view;

pub use call::{CallContext, CallResult, dispatch};

// ─── Canonical addresses ──────────────────────────────────────────────

/// Canonical Arkiv address — the address the precompile is registered
/// at by the custom `EvmFactory`. EOAs / SDKs `CALL` this address with
/// the `execute(Operation[])` / `nonces(address)` ABI declared by
/// `IEntityRegistry`. The precompile itself touches no storage on this
/// address — consensus state lives on the system account.
///
/// Matches the SDK's `ARKIV_ADDRESS` constant. `arkiv-genesis`
/// re-exports it.
pub const ARKIV_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x44,
]);

/// Address the precompile uses as a storage host — global entity
/// counter, per-caller `nonces` map, and the trie-committed ID ↔
/// address maps live here as storage slots. Materialised lazily on
/// the first storage write via `StateAdapter::ensure_account_persists`
/// (called from [`bump_nonce`]), which bumps the nonce to 1 so EIP-161
/// doesn't prune the account at end-of-tx. No genesis allocation
/// required.
///
/// `pub(crate)` — entitydb is the only crate that should touch this
/// address. External callers go through the op handlers and the
/// `read_nonce` / `bump_nonce` API.
pub(crate) const SYSTEM_ACCOUNT_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x46,
]);

// ─── Address derivations ──────────────────────────────────────────────

/// Entity-account address. Spec: `entity_address = entityKey[:20]`
/// (statedb-design §2.1). The address is a pure identity anchor;
/// content commitment is via `codeHash`.
#[inline]
pub fn entity_address(entity_key: B256) -> Address {
    Address::from_slice(&entity_key.0[..20])
}

/// Pair-account address. Spec: `pair_addr = keccak256("arkiv.pair" || k
/// || 0x00 || v)[:20]` (statedb-design §2.3). The `0x00` separator
/// prevents prefix collisions; annot keys and values must not contain
/// `0x00` (precompile enforces).
pub fn pair_address(annot_key: &[u8], annot_val: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(b"arkiv.pair".len() + annot_key.len() + 1 + annot_val.len());
    buf.extend_from_slice(b"arkiv.pair");
    buf.extend_from_slice(annot_key);
    buf.push(0x00);
    buf.extend_from_slice(annot_val);
    Address::from_slice(&keccak256(buf).0[..20])
}

/// Int-index account address. Tier-2 index for values ≤ 32 bytes.
/// Each storage slot: key = right-padded annotation value (B256),
/// value = `(len+1)` as u32 BE in last 4 bytes (0 = absent).
pub fn int_index_address(attr_key: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(b"arkiv.iidx".len() + attr_key.len());
    buf.extend_from_slice(b"arkiv.iidx");
    buf.extend_from_slice(attr_key);
    Address::from_slice(&keccak256(buf).0[..20])
}

/// String-index account address at the given DFS level.
/// `prefix` is the concatenation of all 32-byte chunks consumed so far.
/// Level 0: `prefix = b""`. Level 1: `prefix = chunk0` (32 bytes). Etc.
pub fn str_level_address(attr_key: &[u8], prefix: &[u8]) -> Address {
    let mut buf =
        Vec::with_capacity(b"arkiv.sidx".len() + attr_key.len() + 1 + prefix.len());
    buf.extend_from_slice(b"arkiv.sidx");
    buf.extend_from_slice(attr_key);
    buf.push(0x00);
    buf.extend_from_slice(prefix);
    Address::from_slice(&keccak256(buf).0[..20])
}

/// Magic byte stored at offset 16 of the B+ tree header slot (slot 0 of
/// `btree_header_address`). Distinguishes the header from a list account
/// (whose slot 0 is a u64 count, so byte 16 is always 0).
pub const BTREE_MAGIC: u8 = 0x42;

/// Maximum number of keys per B+ tree node (leaf or internal).
pub const BTREE_ORDER: usize = 32;

/// Header address for the B+ tree Int-mode Tier-2 index for `attr_key`.
///
/// Slot 0 layout: `[0..7]` = root_node_id (u64 BE), `[8..15]` = next_node_id
/// (u64 BE), `[16]` = `BTREE_MAGIC`.
pub fn btree_header_address(attr_key: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(b"arkiv.ibth".len() + attr_key.len());
    buf.extend_from_slice(b"arkiv.ibth");
    buf.extend_from_slice(attr_key);
    Address::from_slice(&keccak256(buf).0[..20])
}

fn btree_node_address(header_addr: &Address, node_id: u64) -> Address {
    let mut buf = [0u8; 38];
    buf[..10].copy_from_slice(b"arkiv.ibtn");
    buf[10..30].copy_from_slice(header_addr.as_slice());
    buf[30..].copy_from_slice(&node_id.to_be_bytes());
    Address::from_slice(&keccak256(buf).0[..20])
}

// ─── Built-in annotation keys ─────────────────────────────────────────
//
// Every entity carries these implicit pairs in addition to its
// user-supplied annotations. The op handlers derive them from the op
// inputs (no caller input needed).

/// Universal "every entity" annotation — every entity is in
/// `("$all", "")`'s bitmap. Lets clients enumerate all entities via a
/// single bitmap read.
pub const ANNOT_ALL: &[u8] = b"$all";

/// `("$creator", creator_address)` — set on `Create`, immutable.
pub const ANNOT_CREATOR: &[u8] = b"$creator";

/// `("$createdAtBlock", be_block_number)` — set on `Create`, immutable.
pub const ANNOT_CREATED_AT_BLOCK: &[u8] = b"$createdAtBlock";

/// `("$owner", owner_address)` — set on `Create`, mutated on
/// `Transfer`.
pub const ANNOT_OWNER: &[u8] = b"$owner";

/// `("$key", entityKey)` — set on `Create`, immutable.
pub const ANNOT_KEY: &[u8] = b"$key";

/// `("$expiration", be_block_number)` — set on `Create`, mutated on
/// `Extend`. Encoded as fixed-width big-endian uint64 so lex order
/// matches numeric order (needed for range scans).
pub const ANNOT_EXPIRATION: &[u8] = b"$expiration";

/// `("$contentType", content_type_bytes)` — set on `Create`, mutated on
/// `Update`.
pub const ANNOT_CONTENT_TYPE: &[u8] = b"$contentType";

// ─── System-account storage slots ─────────────────────────────────────
//
// All four maps live as storage on [`SYSTEM_ACCOUNT_ADDRESS`]. Slot
// keys are scoped by a short tag so the keyspaces can't collide.
// `pub(crate)` so the slot layout stays an entitydb implementation
// detail — external callers go through [`read_nonce`] / [`bump_nonce`]
// and the op handlers.

/// `slot[keccak256("entity_count")]` → next `entity_id` (uint64).
pub(crate) fn slot_entity_count() -> B256 {
    keccak256(b"entity_count")
}

/// `slot[keccak256("id_to_addr" || id_be_bytes)]` → entity_address.
pub(crate) fn slot_id_to_addr(entity_id: u64) -> B256 {
    let mut buf = [0u8; 10 + 8];
    buf[..10].copy_from_slice(b"id_to_addr");
    buf[10..].copy_from_slice(&entity_id.to_be_bytes());
    keccak256(buf)
}

/// `slot[keccak256("addr_to_id" || entity_address_bytes)]` → uint64 ID.
pub(crate) fn slot_addr_to_id(entity_addr: Address) -> B256 {
    let mut buf = [0u8; 10 + 20];
    buf[..10].copy_from_slice(b"addr_to_id");
    buf[10..].copy_from_slice(entity_addr.as_slice());
    keccak256(buf)
}

/// `slot[keccak256("nonces" || caller_address)]` → uint32 entity-key
/// minting nonce, returned by the SDK-visible `nonces(address)` view.
pub(crate) fn slot_nonces(caller: Address) -> B256 {
    let mut buf = [0u8; 6 + 20];
    buf[..6].copy_from_slice(b"nonces");
    buf[6..].copy_from_slice(caller.as_slice());
    keccak256(buf)
}

// ─── Public system-state accessors ────────────────────────────────────

/// Read `caller`'s current entity-key minting nonce. Used by the
/// `nonces(address)` view dispatched from the precompile, and as the
/// `nonce` input to `entityKey` derivation in CREATE.
pub fn read_nonce<S: StateAdapter>(state: &mut S, caller: Address) -> Result<u32> {
    let raw = state.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_nonces(caller))?;
    Ok(u32::from_be_bytes(raw.0[28..].try_into().unwrap()))
}

/// Read-then-increment `caller`'s nonce. Returns the value that was
/// there before the increment (the value to use for the entity-key
/// derivation that's about to happen).
///
/// Also lazily materialises the system account: on the first call
/// against a fresh chain, `ensure_account_persists` raises the system
/// account's nonce to 1 so EIP-161 doesn't prune it (and the nonce
/// slot we're about to write) at end-of-tx. Idempotent on subsequent
/// calls. This is the only entry point that touches the system
/// account before any other slot has been written, so it's enough to
/// run the guard here.
pub fn bump_nonce<S: StateAdapter>(state: &mut S, caller: Address) -> Result<u32> {
    state.ensure_account_persists(&SYSTEM_ACCOUNT_ADDRESS)?;
    let slot = slot_nonces(caller);
    let raw = state.storage(&SYSTEM_ACCOUNT_ADDRESS, slot)?;
    let current = u32::from_be_bytes(raw.0[28..].try_into().unwrap());
    let next = current
        .checked_add(1)
        .ok_or_else(|| eyre::eyre!("nonce overflow for {caller}"))?;
    let mut buf = [0u8; 32];
    buf[28..].copy_from_slice(&next.to_be_bytes());
    state.set_storage(&SYSTEM_ACCOUNT_ADDRESS, slot, B256::from(buf))?;
    Ok(current)
}

// ─── Storage value encodings (for system-account slots) ──────────────

#[inline]
fn u64_to_storage(n: u64) -> B256 {
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&n.to_be_bytes());
    B256::from(buf)
}

#[inline]
fn storage_to_u64(b: B256) -> u64 {
    u64::from_be_bytes(b.0[24..].try_into().unwrap())
}

#[inline]
fn address_to_storage(addr: Address) -> B256 {
    let mut buf = [0u8; 32];
    buf[12..].copy_from_slice(addr.as_slice());
    B256::from(buf)
}

// ─── Tier-2 index slot encodings ─────────────────────────────────────
//
// Slot key   = right-padded annotation value (up to 32 bytes → B256).
// Slot value = 0 (absent) or (len + 1) as u32 big-endian in the last
//              4 bytes of a 32-byte word. `len + 1` avoids ambiguity:
//              0 means absent, 1 means present with len = 0 (empty
//              value), etc. Values cannot contain null bytes (enforced
//              by the precompile), so right-padding is unambiguous.

/// Encode an annotation value (≤ 32 bytes) as a B256 slot key by
/// right-padding with zeros.
#[inline]
pub fn annot_val_to_slot(val: &[u8]) -> B256 {
    debug_assert!(val.len() <= 32, "annot_val_to_slot: value too long ({} bytes)", val.len());
    let mut buf = [0u8; 32];
    buf[..val.len()].copy_from_slice(val);
    B256::from(buf)
}

/// Encode "value of length `len` is present" as a slot value.
/// `slot_presence(0)` = `[0,…,0,0,0,0,1]` (last byte = 1).
#[inline]
pub fn slot_presence(len: usize) -> B256 {
    let mut buf = [0u8; 32];
    buf[28..].copy_from_slice(&(len as u32 + 1).to_be_bytes());
    B256::from(buf)
}

/// Decode the original annotation value length from a slot value.
/// Returns `None` if the slot is zero (absent).
#[inline]
pub fn slot_to_val_len(b: B256) -> Option<usize> {
    let n = u32::from_be_bytes(b.0[28..].try_into().unwrap());
    if n == 0 { None } else { Some((n - 1) as usize) }
}

/// Split an annotation value (≤ 128 bytes) into up to four 32-byte
/// right-padded chunks (B256). An empty value produces a single
/// `B256::ZERO` chunk.
pub fn value_chunks(val: &[u8]) -> Vec<B256> {
    debug_assert!(val.len() <= 128, "value_chunks: value too long ({} bytes)", val.len());
    if val.is_empty() {
        return vec![B256::ZERO];
    }
    let n = val.len().div_ceil(32);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let start = i * 32;
        let end = (start + 32).min(val.len());
        let mut chunk = [0u8; 32];
        chunk[..end - start].copy_from_slice(&val[start..end]);
        out.push(B256::from(chunk));
    }
    out
}

/// Address of the enumeration list for a Tier-2 index address.
///
/// Each Tier-2 index address (`int_index_address` or `str_level_address`)
/// has a companion list address that records, in insertion order, every
/// distinct slot key ever written to the index. This lets `iter_storage_asc`
/// enumerate all live entries without needing a B-tree cursor over the
/// underlying database.
///
/// List layout (storage of the list address):
/// - Slot `B256::ZERO` → count of entries (u64 in last 8 bytes).
/// - Slot `list_entry_slot(i)` for i ∈ 1..=count → the i-th slot key.
pub fn list_address_for(index_addr: &Address) -> Address {
    let mut buf = Vec::with_capacity(b"arkiv.list".len() + 20);
    buf.extend_from_slice(b"arkiv.list");
    buf.extend_from_slice(index_addr.as_slice());
    Address::from_slice(&keccak256(buf).0[..20])
}

/// Slot within a list address that holds the i-th entry (1-based).
/// Slot 0 is reserved for the count.
#[inline]
pub fn list_entry_slot(i: u64) -> B256 {
    u64_to_storage(i)
}

/// Return the exclusive upper-bound B256 slot key for a prefix match:
/// the smallest slot key that is NOT prefixed by `prefix`.
/// Returns `None` if `prefix` is all 0xFF bytes (every slot is a match).
pub fn next_prefix_bound_slot(prefix: &[u8]) -> Option<B256> {
    let mut bound = [0u8; 32];
    bound[..prefix.len()].copy_from_slice(prefix);
    // Increment the big-endian integer formed by the prefix bytes.
    let mut carry = true;
    for b in bound[..prefix.len()].iter_mut().rev() {
        if carry {
            if *b == 0xFF {
                *b = 0;
            } else {
                *b += 1;
                carry = false;
            }
        }
    }
    if carry { None } else { Some(B256::from(bound)) }
}

// ─── Annotation value encodings (for pair-account addresses) ─────────
//
// Encoding choices are critical: lex order of these byte sequences
// must match the intended ordering for range queries. For numeric
// values (block numbers, uint annotations) that means fixed-width
// big-endian. For addresses it doesn't matter (range queries on
// addresses don't make sense); we use the natural 20-byte form.

#[inline]
fn encode_u64_be(n: u64) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

#[inline]
fn encode_address(addr: Address) -> Vec<u8> {
    addr.as_slice().to_vec()
}

#[inline]
fn encode_b256(b: B256) -> Vec<u8> {
    b.0.to_vec()
}

// ─── Bitmap (roaring64) ───────────────────────────────────────────────

/// Roaring64 bitmap of entity IDs.
///
/// Determinism guarantee: [`Bitmap::to_bytes`] produces the same bytes
/// for any two instances that contain the same set of IDs. Required
/// for `codeHash = keccak256(bitmap_bytes)` to agree across nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bitmap(RoaringTreemap);

impl Bitmap {
    pub fn new() -> Self {
        Self(RoaringTreemap::new())
    }

    /// Deserialize from the portable RoaringFormatSpec layout.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        RoaringTreemap::deserialize_from(bytes)
            .map(Self)
            .map_err(|e| eyre::eyre!("invalid roaring bitmap bytes: {e}"))
    }

    /// Serialize to the portable RoaringFormatSpec layout. Same set →
    /// same bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.0.serialized_size());
        self.0
            .serialize_into(&mut buf)
            .expect("writing to Vec is infallible");
        buf
    }

    pub fn insert(&mut self, id: u64) -> bool {
        self.0.insert(id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        self.0.remove(id)
    }

    pub fn contains(&self, id: u64) -> bool {
        self.0.contains(id)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter()
    }

    /// In-place set union: `self ∪= other`.
    pub fn union_with(&mut self, other: &Bitmap) {
        self.0 |= &other.0;
    }

    /// In-place set intersection: `self ∩= other`.
    pub fn intersect_with(&mut self, other: &Bitmap) {
        self.0 &= &other.0;
    }

    /// In-place set difference: `self \= other`.
    pub fn subtract(&mut self, other: &Bitmap) {
        self.0 -= &other.0;
    }
}

impl FromIterator<u64> for Bitmap {
    fn from_iter<I: IntoIterator<Item = u64>>(iter: I) -> Self {
        Self(RoaringTreemap::from_iter(iter))
    }
}

// ─── Tier-2 index mode ────────────────────────────────────────────────

/// Whether an annotation key uses the single-level **int** index
/// (values ≤ 32 bytes, one slot per value) or the four-level **string**
/// cascade index (values ≤ 128 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnnotMode {
    /// Single-level index at `int_index_address(attr_key)`. Slot key =
    /// right-padded 32-byte value; slot value = `(len+1)` as u32 BE in
    /// last 4 bytes.
    Int,
    /// Four-level cascade at `str_level_address(attr_key, prefix)`.
    /// Each level's slot key is the next 32-byte chunk of the value.
    Str,
}

// ─── Entity RLP ───────────────────────────────────────────────────────

/// Prefix prepended to the RLP bytes before storing as account `code`.
/// `0xFE` is the EVM `INVALID` opcode — any `CALL` to an entity
/// address halts immediately.
pub const ENTITY_CODE_PREFIX: u8 = 0xFE;

/// Attribute `value_type` tags. Must match
/// `Entity.ATTR_{UINT,STRING,ENTITY_KEY}` in EntityRegistry.sol and
/// the ABI shape decoded by the precompile.
pub const ATTR_UINT: u8 = 1;
pub const ATTR_STRING: u8 = 2;
pub const ATTR_ENTITY_KEY: u8 = 3;

/// On-trie representation of an entity. Encoded as
/// `0xFE || RLP(EntityRlp)` and stored as the entity-account `code`.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct EntityRlp {
    pub payload: Vec<u8>,
    pub creator: Address,
    pub created_at_block: u64,
    pub owner: Address,
    pub expires_at: u64,
    pub content_type: Vec<u8>,
    pub key: B256,
    pub attributes: Vec<Attribute>,
    /// Block number of the most recent mutation (CREATE / UPDATE /
    /// EXTEND / TRANSFER) — equals `created_at_block` until the
    /// entity is first modified.
    pub last_modified_at_block: u64,
}

/// Discriminated `(key, value)` attribute mirroring the precompile
/// ABI. `value_type` selects how `value` should be interpreted:
/// `ATTR_UINT` → 32-byte big-endian uint256; `ATTR_STRING` → opaque
/// bytes (UTF-8 by SDK convention); `ATTR_ENTITY_KEY` → 32 raw
/// bytes of an entity key.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct Attribute {
    pub key: Vec<u8>,
    pub value_type: u8,
    pub value: Vec<u8>,
}

impl EntityRlp {
    /// Encode for storage as account code: `0xFE || RLP(self)`.
    pub fn encode_as_code(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + self.length());
        buf.push(ENTITY_CODE_PREFIX);
        self.encode(&mut buf);
        buf
    }

    /// Decode from account code. Verifies the `0xFE` prefix and then
    /// RLP-decodes the rest.
    pub fn decode_from_code(code: &[u8]) -> Result<Self> {
        ensure!(
            code.first() == Some(&ENTITY_CODE_PREFIX),
            "entity code is missing the {:#x} prefix",
            ENTITY_CODE_PREFIX,
        );
        let mut rest = &code[1..];
        Self::decode(&mut rest).map_err(|e| eyre::eyre!("RLP decode of EntityRlp failed: {e}"))
    }
}

// ─── State adapter trait ──────────────────────────────────────────────

/// Abstract state interface the op handlers run against.
///
/// In production, [`arkiv_node::precompile`] implements this over
/// revm's `EvmInternals`. For tests, [`test_utils::InMemoryStateAdapter`]
/// implements it over an [`test_utils::InMemoryStateDb`].
///
/// Conventions:
/// - `code` returns an empty `Vec` for absent / empty-coded accounts.
/// - `set_code` creates the account if needed and sets `nonce = 1` if
///   it was previously zero.
/// - `tombstone_code` clears the code but preserves `nonce = 1` so
///   EIP-161 doesn't prune the account.
/// - `ensure_account_persists` raises the account's nonce to at least
///   1 so EIP-161 doesn't prune it at end-of-tx. Idempotent. Used by
///   the entitydb to lazily materialise the system account on its
///   first storage write — without it, an empty-coded account that
///   only receives storage writes is still EIP-161-empty (the check
///   ignores storage) and gets pruned along with its slots.
pub trait StateAdapter {
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>>;
    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()>;
    fn tombstone_code(&mut self, addr: &Address) -> Result<()>;
    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256>;
    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()>;
    fn ensure_account_persists(&mut self, addr: &Address) -> Result<()>;

    /// Return all storage entries for `addr` with slot key ≥ `from`,
    /// ordered ascending by slot key. Used by range queries to iterate
    /// the Tier-2 index accounts. Write-time implementations may bail.
    fn iter_storage_asc(&mut self, addr: &Address, from: B256) -> Result<Vec<(B256, B256)>>;
}

// ─── Op handlers ──────────────────────────────────────────────────────
//
// Each handler assumes the contract has already validated ownership /
// liveness. It performs all the state mutations: system-account
// counter, ID maps, bitmap deltas (built-in + user annotations), and
// the entity-account RLP write.

/// Create a new entity. Allocates a fresh `entity_id`, writes both ID
/// maps on the Arkiv account, populates all built-in + user bitmaps,
/// and writes the entity RLP.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "entitydb_create",
    level = "debug",
    skip_all,
    fields(
        payload_bytes = payload.len(),
        n_attrs = attributes.len(),
    ),
)]
pub fn create<S: StateAdapter>(
    state: &mut S,
    sender: Address,
    entity_key: B256,
    expires_at: u64,
    current_block: u64,
    payload: Vec<u8>,
    content_type: Vec<u8>,
    attributes: Vec<Attribute>,
) -> Result<()> {
    // 1) Allocate entity_id.
    let count_slot = slot_entity_count();
    let prev = state.storage(&SYSTEM_ACCOUNT_ADDRESS, count_slot)?;
    let entity_id = storage_to_u64(prev);
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        count_slot,
        u64_to_storage(entity_id + 1),
    )?;

    // 2) Write ID maps.
    let entity_addr = entity_address(entity_key);
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        slot_id_to_addr(entity_id),
        address_to_storage(entity_addr),
    )?;
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        slot_addr_to_id(entity_addr),
        u64_to_storage(entity_id),
    )?;

    // 3) Insert into every bitmap (built-in + user).
    for a in built_in_annotations(
        sender,
        sender,
        entity_key,
        current_block,
        expires_at,
        &content_type,
    )
    .into_iter()
    .chain(user_annotations(&attributes))
    {
        insert_into_indexes(state, &a.key, &a.value, entity_id, a.mode)?;
    }

    // 4) Write the entity RLP.
    let entity = EntityRlp {
        payload,
        creator: sender,
        created_at_block: current_block,
        owner: sender,
        expires_at,
        content_type,
        key: entity_key,
        attributes,
        last_modified_at_block: current_block,
    };
    state.set_code(&entity_addr, entity.encode_as_code())?;

    Ok(())
}

/// Replace an entity's payload / content type / annotations.
///
/// Preserves `creator`, `created_at_block`, `key`, `owner`,
/// `expires_at`. Bitmap diff: only annotations that changed get
/// touched (incl. `$contentType` if the content type changed).
#[tracing::instrument(
    name = "entitydb_update",
    level = "debug",
    skip_all,
    fields(
        payload_bytes = payload.len(),
        n_attrs = attributes.len(),
    ),
)]
pub fn update<S: StateAdapter>(
    state: &mut S,
    entity_key: B256,
    current_block: u64,
    payload: Vec<u8>,
    content_type: Vec<u8>,
    attributes: Vec<Attribute>,
) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = read_entity_id(state, entity_addr)?;
    let mut entity = read_entity(state, entity_addr)?;

    // Annotation diff. Built-ins that don't change on UPDATE
    // (`$creator`, `$createdAtBlock`, `$key`, `$owner`, `$expiration`,
    // `$all`) aren't included on either side, so the diff doesn't
    // touch them. `$contentType` IS in the diff so it moves if the
    // content type changed.
    let old_annotations = updatable_annotations(&entity.content_type, &entity.attributes);
    let new_annotations = updatable_annotations(&content_type, &attributes);
    apply_annotation_diff(state, &old_annotations, &new_annotations, entity_id)?;

    entity.payload = payload;
    entity.content_type = content_type;
    entity.attributes = attributes;
    entity.last_modified_at_block = current_block;
    state.set_code(&entity_addr, entity.encode_as_code())?;

    Ok(())
}

/// Extend an entity's `expires_at`. Updates the `$expiration` bitmap
/// and re-encodes the RLP with the new value.
#[tracing::instrument(name = "entitydb_extend", level = "debug", skip_all)]
pub fn extend<S: StateAdapter>(
    state: &mut S,
    entity_key: B256,
    current_block: u64,
    new_expires_at: u64,
) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = read_entity_id(state, entity_addr)?;
    let mut entity = read_entity(state, entity_addr)?;

    remove_from_indexes(
        state,
        ANNOT_EXPIRATION,
        &encode_u64_be(entity.expires_at),
        entity_id,
        Some(AnnotMode::Int),
    )?;
    insert_into_indexes(
        state,
        ANNOT_EXPIRATION,
        &encode_u64_be(new_expires_at),
        entity_id,
        Some(AnnotMode::Int),
    )?;

    entity.expires_at = new_expires_at;
    entity.last_modified_at_block = current_block;
    state.set_code(&entity_addr, entity.encode_as_code())?;

    Ok(())
}

/// Hand an entity's ownership to `new_owner`. Updates the `$owner`
/// bitmap and re-encodes the RLP.
#[tracing::instrument(name = "entitydb_transfer", level = "debug", skip_all)]
pub fn transfer<S: StateAdapter>(
    state: &mut S,
    entity_key: B256,
    current_block: u64,
    new_owner: Address,
) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = read_entity_id(state, entity_addr)?;
    let mut entity = read_entity(state, entity_addr)?;

    remove_from_indexes(state, ANNOT_OWNER, &encode_address(entity.owner), entity_id, None)?;
    insert_into_indexes(state, ANNOT_OWNER, &encode_address(new_owner), entity_id, None)?;

    entity.owner = new_owner;
    entity.last_modified_at_block = current_block;
    state.set_code(&entity_addr, entity.encode_as_code())?;

    Ok(())
}

/// Remove an entity. Clears every bitmap entry (built-in + user),
/// clears both ID-map slots on the Arkiv account, and tombstones the
/// entity account (`code = nil`, `nonce = 1`).
#[tracing::instrument(name = "entitydb_delete", level = "debug", skip_all)]
pub fn delete<S: StateAdapter>(state: &mut S, entity_key: B256) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = read_entity_id(state, entity_addr)?;
    let entity = read_entity(state, entity_addr)?;

    for a in built_in_annotations(
        entity.creator,
        entity.owner,
        entity_key,
        entity.created_at_block,
        entity.expires_at,
        &entity.content_type,
    )
    .into_iter()
    .chain(user_annotations(&entity.attributes))
    {
        remove_from_indexes(state, &a.key, &a.value, entity_id, a.mode)?;
    }

    // Clear ID-map slots.
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        slot_id_to_addr(entity_id),
        B256::ZERO,
    )?;
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        slot_addr_to_id(entity_addr),
        B256::ZERO,
    )?;

    // Tombstone — keeps nonce=1 to defeat EIP-161.
    state.tombstone_code(&entity_addr)?;

    Ok(())
}

/// Identical state path to [`delete`]. The contract has already
/// validated `block.number > expiresAt`.
#[tracing::instrument(name = "entitydb_expire", level = "debug", skip_all)]
pub fn expire<S: StateAdapter>(state: &mut S, entity_key: B256) -> Result<()> {
    delete(state, entity_key)
}

// ─── Internal helpers ─────────────────────────────────────────────────

fn read_entity<S: StateAdapter>(state: &mut S, entity_addr: Address) -> Result<EntityRlp> {
    let code = state.code(&entity_addr)?;
    ensure!(!code.is_empty(), "no entity at {entity_addr}");
    EntityRlp::decode_from_code(&code)
}

fn read_entity_id<S: StateAdapter>(state: &mut S, entity_addr: Address) -> Result<u64> {
    let slot = slot_addr_to_id(entity_addr);
    Ok(storage_to_u64(
        state.storage(&SYSTEM_ACCOUNT_ADDRESS, slot)?,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Annotation {
    key: Vec<u8>,
    value: Vec<u8>,
    /// `Some(mode)` — maintain a tier-2 range/glob index using `mode`.
    /// `None` — random-distribution value; tier-1 pair bitmap only.
    mode: Option<AnnotMode>,
}

/// All built-in annotations for an entity. Used by `create` (to insert)
/// and `delete` / `expire` (to remove).
fn built_in_annotations(
    creator: Address,
    owner: Address,
    entity_key: B256,
    created_at_block: u64,
    expires_at: u64,
    content_type: &[u8],
) -> Vec<Annotation> {
    vec![
        Annotation { key: ANNOT_ALL.to_vec(),              value: Vec::new(),                        mode: None },
        Annotation { key: ANNOT_CREATOR.to_vec(),          value: encode_address(creator),           mode: None },
        Annotation { key: ANNOT_OWNER.to_vec(),            value: encode_address(owner),             mode: None },
        Annotation { key: ANNOT_KEY.to_vec(),              value: encode_b256(entity_key),           mode: None },
        Annotation { key: ANNOT_CREATED_AT_BLOCK.to_vec(), value: encode_u64_be(created_at_block),   mode: Some(AnnotMode::Int) },
        Annotation { key: ANNOT_EXPIRATION.to_vec(),       value: encode_u64_be(expires_at),         mode: Some(AnnotMode::Int) },
        Annotation { key: ANNOT_CONTENT_TYPE.to_vec(),     value: content_type.to_vec(),             mode: Some(AnnotMode::Str) },
    ]
}

/// User-supplied attributes flattened to annotations.
/// `ATTR_UINT` and `ATTR_STRING` get a tier-2 index; `ATTR_ENTITY_KEY`
/// (random 32-byte hash) does not.
fn user_annotations<'a>(
    attributes: &'a [Attribute],
) -> impl Iterator<Item = Annotation> + 'a {
    attributes.iter().map(|a| Annotation {
        key: a.key.clone(),
        value: a.value.clone(),
        mode: match a.value_type {
            ATTR_UINT => Some(AnnotMode::Int),
            ATTR_STRING => Some(AnnotMode::Str),
            _ => None,
        },
    })
}

/// Annotations that an UPDATE op diffs: user attributes plus `$contentType`.
fn updatable_annotations(content_type: &[u8], attributes: &[Attribute]) -> Vec<Annotation> {
    let mut out = Vec::with_capacity(1 + attributes.len());
    out.push(Annotation {
        key: ANNOT_CONTENT_TYPE.to_vec(),
        value: content_type.to_vec(),
        mode: Some(AnnotMode::Str),
    });
    out.extend(user_annotations(attributes));
    out
}

/// Diff two annotation sets and apply removals + insertions to tier-1 pair
/// bitmaps and (where `mode` is `Some`) the corresponding tier-2 index.
fn apply_annotation_diff<S: StateAdapter>(
    state: &mut S,
    old: &[Annotation],
    new: &[Annotation],
    entity_id: u64,
) -> Result<()> {
    use std::collections::BTreeSet;
    let old_set: BTreeSet<&Annotation> = old.iter().collect();
    let new_set: BTreeSet<&Annotation> = new.iter().collect();
    for a in old.iter().filter(|a| !new_set.contains(*a)) {
        remove_from_indexes(state, &a.key, &a.value, entity_id, a.mode)?;
    }
    for a in new.iter().filter(|a| !old_set.contains(*a)) {
        insert_into_indexes(state, &a.key, &a.value, entity_id, a.mode)?;
    }
    Ok(())
}

fn insert_into_indexes<S: StateAdapter>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
    entity_id: u64,
    mode: Option<AnnotMode>,
) -> Result<()> {
    let mut bitmap = read_pair_bitmap(state, annot_key, annot_val)?;
    let was_empty = bitmap.is_empty();
    bitmap.insert(entity_id);
    state.set_code(&pair_address(annot_key, annot_val), bitmap.to_bytes())?;
    if let Some(mode) = mode {
        if was_empty {
            tier2_insert(state, annot_key, annot_val, mode)?;
        }
    }
    Ok(())
}

fn remove_from_indexes<S: StateAdapter>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
    entity_id: u64,
    mode: Option<AnnotMode>,
) -> Result<()> {
    let mut bitmap = read_pair_bitmap(state, annot_key, annot_val)?;
    if bitmap.is_empty() {
        return Ok(());
    }
    bitmap.remove(entity_id);
    state.set_code(&pair_address(annot_key, annot_val), bitmap.to_bytes())?;
    if let Some(mode) = mode {
        if bitmap.is_empty() {
            tier2_remove(state, annot_key, annot_val, mode)?;
        }
    }
    Ok(())
}

/// Append `entry` (a slot key) to the enumeration list for `index_addr`.
/// Called exactly once per distinct value, either on first Int insert
/// (guaranteed by `was_empty`) or on first appearance of a new chunk at
/// a given Str level (guarded by a pre-read in `tier2_insert`).
fn list_append<S: StateAdapter>(
    state: &mut S,
    index_addr: &Address,
    entry: B256,
) -> Result<()> {
    let list_addr = list_address_for(index_addr);
    state.ensure_account_persists(&list_addr)?;
    let count = storage_to_u64(state.storage(&list_addr, B256::ZERO)?);
    state.set_storage(&list_addr, list_entry_slot(count + 1), entry)?;
    state.set_storage(&list_addr, B256::ZERO, u64_to_storage(count + 1))?;
    Ok(())
}

// ─── In-storage B+ tree (Int-mode Tier-2 index) ──────────────────────

#[inline]
fn btree_key_slot(i: usize) -> B256 {
    u64_to_storage(1 + i as u64)
}

#[inline]
fn btree_value_slot(i: usize) -> B256 {
    u64_to_storage(1 + BTREE_ORDER as u64 + i as u64)
}

struct BTreeNode {
    is_leaf: bool,
    right_sibling: u64,
    keys: Vec<B256>,
    values: Vec<B256>,
}

fn btree_read_header<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
) -> Result<(u64, u64)> {
    let raw = state.storage(header_addr, B256::ZERO)?;
    let root_id = u64::from_be_bytes(raw.0[0..8].try_into().unwrap());
    let next_id = u64::from_be_bytes(raw.0[8..16].try_into().unwrap());
    Ok((root_id, next_id))
}

fn btree_write_header<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    root_id: u64,
    next_id: u64,
) -> Result<()> {
    let mut buf = [0u8; 32];
    buf[0..8].copy_from_slice(&root_id.to_be_bytes());
    buf[8..16].copy_from_slice(&next_id.to_be_bytes());
    buf[16] = BTREE_MAGIC;
    state.ensure_account_persists(header_addr)?;
    state.set_storage(header_addr, B256::ZERO, B256::from(buf))
}

/// Allocates a new node ID: reads current next_id from header, writes
/// next_id+1 back (preserving root_id), returns the allocated ID.
fn btree_alloc_node<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
) -> Result<u64> {
    let (root_id, next_id) = btree_read_header(state, header_addr)?;
    let node_id = if next_id == 0 { 1 } else { next_id };
    btree_write_header(state, header_addr, root_id, node_id + 1)?;
    Ok(node_id)
}

fn btree_read_node<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    node_id: u64,
) -> Result<BTreeNode> {
    let node_addr = btree_node_address(header_addr, node_id);
    let meta = state.storage(&node_addr, B256::ZERO)?;
    let right_sibling = u64::from_be_bytes(meta.0[0..8].try_into().unwrap());
    let key_count = u16::from_be_bytes(meta.0[8..10].try_into().unwrap()) as usize;
    let is_leaf = meta.0[10] & 1 != 0;

    let mut keys = Vec::with_capacity(key_count);
    for i in 0..key_count {
        keys.push(state.storage(&node_addr, btree_key_slot(i))?);
    }
    // Leaves: key_count values; internals: key_count+1 children.
    let val_count = if is_leaf { key_count } else { key_count + 1 };
    let mut values = Vec::with_capacity(val_count);
    for i in 0..val_count {
        values.push(state.storage(&node_addr, btree_value_slot(i))?);
    }
    Ok(BTreeNode { is_leaf, right_sibling, keys, values })
}

fn btree_write_node<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    node_id: u64,
    node: &BTreeNode,
) -> Result<()> {
    let node_addr = btree_node_address(header_addr, node_id);
    state.ensure_account_persists(&node_addr)?;
    let mut meta = [0u8; 32];
    meta[0..8].copy_from_slice(&node.right_sibling.to_be_bytes());
    meta[8..10].copy_from_slice(&(node.keys.len() as u16).to_be_bytes());
    meta[10] = if node.is_leaf { 1 } else { 0 };
    state.set_storage(&node_addr, B256::ZERO, B256::from(meta))?;
    for (i, k) in node.keys.iter().enumerate() {
        state.set_storage(&node_addr, btree_key_slot(i), *k)?;
    }
    for (i, v) in node.values.iter().enumerate() {
        state.set_storage(&node_addr, btree_value_slot(i), *v)?;
    }
    Ok(())
}

fn btree_descend_to_leaf<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    mut node_id: u64,
    key: B256,
) -> Result<u64> {
    loop {
        let node = btree_read_node(state, header_addr, node_id)?;
        if node.is_leaf {
            return Ok(node_id);
        }
        // Among children, `k <= key` counts how many keys precede the target
        // child; that count is the correct child index.
        let pos = node.keys.partition_point(|k| *k <= key);
        node_id = u64::from_be_bytes(node.values[pos].0[24..32].try_into().unwrap());
    }
}

enum InsertResult {
    Done,
    Split { push_up: B256, new_sibling_id: u64 },
}

fn insert_recursive<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    node_id: u64,
    key: B256,
    value: B256,
) -> Result<InsertResult> {
    let mut node = btree_read_node(state, header_addr, node_id)?;

    if node.is_leaf {
        let pos = node.keys.partition_point(|k| *k < key);
        if pos < node.keys.len() && node.keys[pos] == key {
            // Update in place (handles lazy-delete re-insert and B256::ZERO $all key).
            node.values[pos] = value;
            btree_write_node(state, header_addr, node_id, &node)?;
            return Ok(InsertResult::Done);
        }
        node.keys.insert(pos, key);
        node.values.insert(pos, value);
        if node.keys.len() <= BTREE_ORDER {
            btree_write_node(state, header_addr, node_id, &node)?;
            return Ok(InsertResult::Done);
        }
        let mid = node.keys.len() / 2;
        let new_id = btree_alloc_node(state, header_addr)?;
        let new_node = BTreeNode {
            is_leaf: true,
            right_sibling: node.right_sibling,
            keys: node.keys.split_off(mid),
            values: node.values.split_off(mid),
        };
        let push_up = new_node.keys[0];
        node.right_sibling = new_id;
        btree_write_node(state, header_addr, node_id, &node)?;
        btree_write_node(state, header_addr, new_id, &new_node)?;
        return Ok(InsertResult::Split { push_up, new_sibling_id: new_id });
    }

    // Internal node
    let pos = node.keys.partition_point(|k| *k <= key);
    let child_id = u64::from_be_bytes(node.values[pos].0[24..32].try_into().unwrap());
    match insert_recursive(state, header_addr, child_id, key, value)? {
        InsertResult::Done => Ok(InsertResult::Done),
        InsertResult::Split { push_up, new_sibling_id } => {
            node.keys.insert(pos, push_up);
            let mut rhs_buf = [0u8; 32];
            rhs_buf[24..32].copy_from_slice(&new_sibling_id.to_be_bytes());
            node.values.insert(pos + 1, B256::from(rhs_buf));
            if node.keys.len() <= BTREE_ORDER {
                btree_write_node(state, header_addr, node_id, &node)?;
                return Ok(InsertResult::Done);
            }
            let mid = node.keys.len() / 2;
            let push_up_key = node.keys[mid];
            let new_id = btree_alloc_node(state, header_addr)?;
            let new_node = BTreeNode {
                is_leaf: false,
                right_sibling: node.right_sibling,
                keys: node.keys.split_off(mid + 1),
                values: node.values.split_off(mid + 1),
            };
            node.keys.truncate(mid);
            node.right_sibling = new_id;
            btree_write_node(state, header_addr, node_id, &node)?;
            btree_write_node(state, header_addr, new_id, &new_node)?;
            Ok(InsertResult::Split { push_up: push_up_key, new_sibling_id: new_id })
        }
    }
}

fn btree_insert<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    key: B256,
    value: B256,
) -> Result<()> {
    let (root_id, _) = btree_read_header(state, header_addr)?;
    if root_id == 0 {
        let leaf_id = btree_alloc_node(state, header_addr)?;
        let leaf = BTreeNode {
            is_leaf: true,
            right_sibling: 0,
            keys: vec![key],
            values: vec![value],
        };
        btree_write_node(state, header_addr, leaf_id, &leaf)?;
        let (_, next) = btree_read_header(state, header_addr)?;
        return btree_write_header(state, header_addr, leaf_id, next);
    }
    match insert_recursive(state, header_addr, root_id, key, value)? {
        InsertResult::Done => {}
        InsertResult::Split { push_up, new_sibling_id } => {
            let new_root_id = btree_alloc_node(state, header_addr)?;
            let mut lhs_buf = [0u8; 32];
            lhs_buf[24..32].copy_from_slice(&root_id.to_be_bytes());
            let mut rhs_buf = [0u8; 32];
            rhs_buf[24..32].copy_from_slice(&new_sibling_id.to_be_bytes());
            let new_root = BTreeNode {
                is_leaf: false,
                right_sibling: 0,
                keys: vec![push_up],
                values: vec![B256::from(lhs_buf), B256::from(rhs_buf)],
            };
            btree_write_node(state, header_addr, new_root_id, &new_root)?;
            let (_, next) = btree_read_header(state, header_addr)?;
            btree_write_header(state, header_addr, new_root_id, next)?;
        }
    }
    Ok(())
}

fn btree_lazy_delete<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    key: B256,
) -> Result<()> {
    let (root_id, _) = btree_read_header(state, header_addr)?;
    if root_id == 0 {
        return Ok(());
    }
    let leaf_id = btree_descend_to_leaf(state, header_addr, root_id, key)?;
    let node = btree_read_node(state, header_addr, leaf_id)?;
    let pos = node.keys.partition_point(|k| *k < key);
    if pos < node.keys.len() && node.keys[pos] == key {
        let node_addr = btree_node_address(header_addr, leaf_id);
        state.set_storage(&node_addr, btree_value_slot(pos), B256::ZERO)?;
    }
    Ok(())
}

/// Enumerate all B+ tree entries with key ≥ `from`, in ascending key order.
/// Lazy-deleted entries (value = B256::ZERO) are skipped.
pub fn btree_iter_from<S: StateAdapter>(
    state: &mut S,
    header_addr: &Address,
    from: B256,
) -> Result<Vec<(B256, B256)>> {
    let (root_id, _) = btree_read_header(state, header_addr)?;
    if root_id == 0 {
        return Ok(vec![]);
    }
    let mut leaf_id = btree_descend_to_leaf(state, header_addr, root_id, from)?;
    let mut result = Vec::new();
    let mut first = true;
    loop {
        let leaf = btree_read_node(state, header_addr, leaf_id)?;
        let start = if first {
            first = false;
            leaf.keys.partition_point(|k| *k < from)
        } else {
            0
        };
        for i in start..leaf.keys.len() {
            if leaf.values[i] != B256::ZERO {
                result.push((leaf.keys[i], leaf.values[i]));
            }
        }
        if leaf.right_sibling == 0 {
            break;
        }
        leaf_id = leaf.right_sibling;
    }
    Ok(result)
}

/// Enumerate all list-based (Str-mode) Tier-2 index entries for
/// `index_addr` with key ≥ `from`, in ascending key order.
pub fn list_iter_from<S: StateAdapter>(
    state: &mut S,
    index_addr: &Address,
    from: B256,
) -> Result<Vec<(B256, B256)>> {
    let list_addr = list_address_for(index_addr);
    let count = storage_to_u64(state.storage(&list_addr, B256::ZERO)?);
    let mut entries = Vec::with_capacity(count as usize);
    for i in 1..=count {
        let slot_key = state.storage(&list_addr, list_entry_slot(i))?;
        let presence = state.storage(index_addr, slot_key)?;
        if presence != B256::ZERO {
            entries.push((slot_key, presence));
        }
    }
    entries.sort_unstable_by_key(|(k, _)| *k);
    let start = entries.partition_point(|(k, _)| *k < from);
    Ok(entries.into_iter().skip(start).collect())
}

/// Shared `iter_storage_asc` dispatch: reads byte [16] of slot 0 to
/// distinguish a B+ tree header (`BTREE_MAGIC`) from a list account.
pub fn iter_storage_asc_impl<S: StateAdapter>(
    state: &mut S,
    addr: &Address,
    from: B256,
) -> Result<Vec<(B256, B256)>> {
    let slot0 = state.storage(addr, B256::ZERO)?;
    if slot0.0[16] == BTREE_MAGIC {
        btree_iter_from(state, addr, from)
    } else {
        list_iter_from(state, addr, from)
    }
}

fn tier2_insert<S: StateAdapter>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
    mode: AnnotMode,
) -> Result<()> {
    match mode {
        AnnotMode::Int => {
            btree_insert(
                state,
                &btree_header_address(annot_key),
                annot_val_to_slot(annot_val),
                slot_presence(annot_val.len()),
            )?;
        }
        AnnotMode::Str => {
            let chunks = value_chunks(annot_val);
            let mut prefix: Vec<u8> = Vec::with_capacity(96);
            for chunk in &chunks {
                let addr = str_level_address(annot_key, &prefix);
                state.ensure_account_persists(&addr)?;
                // At intermediate levels, the same chunk may appear for
                // multiple values sharing that prefix; only register it
                // in the list on its first appearance at this level.
                let existing = state.storage(&addr, *chunk)?;
                if existing == B256::ZERO {
                    list_append(state, &addr, *chunk)?;
                }
                state.set_storage(&addr, *chunk, slot_presence(annot_val.len()))?;
                prefix.extend_from_slice(chunk.as_slice());
            }
        }
    }
    Ok(())
}

fn tier2_remove<S: StateAdapter>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
    mode: AnnotMode,
) -> Result<()> {
    match mode {
        AnnotMode::Int => {
            btree_lazy_delete(
                state,
                &btree_header_address(annot_key),
                annot_val_to_slot(annot_val),
            )?;
        }
        AnnotMode::Str => {
            let chunks = value_chunks(annot_val);
            let mut prefix: Vec<u8> = Vec::with_capacity(96);
            for chunk in &chunks {
                let addr = str_level_address(annot_key, &prefix);
                state.set_storage(&addr, *chunk, B256::ZERO)?;
                prefix.extend_from_slice(chunk.as_slice());
            }
        }
    }
    Ok(())
}

// ─── Public read-side helpers (used by the query interpreter) ─────────

/// Read the pair-account bitmap for `(annot_key, annot_val)`. An
/// account with empty code (never written, or tombstoned) decodes to
/// an empty [`Bitmap`] — not an error.
pub fn read_pair_bitmap<S: StateAdapter>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
) -> Result<Bitmap> {
    let pair_addr = pair_address(annot_key, annot_val);
    let code = state.code(&pair_addr)?;
    if code.is_empty() {
        Ok(Bitmap::new())
    } else {
        Bitmap::from_bytes(&code)
    }
}

/// Bitmap of every live entity ID — the `$all` built-in bitmap.
/// Convenience wrapper around [`read_pair_bitmap`].
pub fn all_entities<S: StateAdapter>(state: &mut S) -> Result<Bitmap> {
    read_pair_bitmap(state, ANNOT_ALL, b"")
}


/// Resolve a query-hit entity ID to its on-trie [`EntityRlp`].
///
/// Returns `Ok(None)` if the ID's `id_to_addr` slot is zero (never
/// written, or cleared by `delete` / `expire`) or if the entity
/// account has empty code (tombstoned). Returns `Err` only on
/// underlying state errors or malformed entity bytes.
pub fn resolve_id<S: StateAdapter>(state: &mut S, id: u64) -> Result<Option<EntityRlp>> {
    let raw = state.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_id_to_addr(id))?;
    if raw == B256::ZERO {
        return Ok(None);
    }
    let entity_addr = Address::from_slice(&raw.0[12..]);
    let code = state.code(&entity_addr)?;
    if code.is_empty() {
        return Ok(None);
    }
    Ok(Some(EntityRlp::decode_from_code(&code)?))
}

// ─── Test backend (in-memory state DB) ────────────────────────────────

#[cfg(feature = "test-utils")]
pub mod test_utils {
    //! In-memory [`StateAdapter`] implementation, suitable for unit
    //! tests in this crate and for downstream test code that wants to
    //! drive the op handlers without a revm context.

    use super::*;
    use std::collections::HashMap;

    /// Per-account state: nonce, code, and the storage map.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct AccountState {
        pub nonce: u64,
        pub code: Vec<u8>,
        pub storage: HashMap<B256, B256>,
    }

    /// Toy state DB: account address → [`AccountState`]. Stand-in for
    /// revm's state DB during tests.
    #[derive(Debug, Clone, Default)]
    pub struct InMemoryStateDb {
        accounts: HashMap<Address, AccountState>,
    }

    impl InMemoryStateDb {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn account(&self, addr: &Address) -> Option<&AccountState> {
            self.accounts.get(addr)
        }

        pub fn account_mut(&mut self, addr: &Address) -> &mut AccountState {
            self.accounts.entry(*addr).or_default()
        }
    }

    /// Thin [`StateAdapter`] over a borrowed [`InMemoryStateDb`].
    pub struct InMemoryStateAdapter<'a> {
        db: &'a mut InMemoryStateDb,
    }

    impl<'a> InMemoryStateAdapter<'a> {
        pub fn new(db: &'a mut InMemoryStateDb) -> Self {
            Self { db }
        }
    }

    impl StateAdapter for InMemoryStateAdapter<'_> {
        fn code(&mut self, addr: &Address) -> Result<Vec<u8>> {
            Ok(self
                .db
                .account(addr)
                .map(|a| a.code.clone())
                .unwrap_or_default())
        }

        fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()> {
            let acc = self.db.account_mut(addr);
            acc.code = code;
            if acc.nonce == 0 {
                acc.nonce = 1;
            }
            Ok(())
        }

        fn tombstone_code(&mut self, addr: &Address) -> Result<()> {
            let acc = self.db.account_mut(addr);
            acc.code = Vec::new();
            // Preserve nonce >= 1 to defeat EIP-161 pruning.
            if acc.nonce == 0 {
                acc.nonce = 1;
            }
            Ok(())
        }

        fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256> {
            Ok(self
                .db
                .account(addr)
                .and_then(|a| a.storage.get(&slot).copied())
                .unwrap_or_default())
        }

        fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()> {
            self.db.account_mut(addr).storage.insert(slot, value);
            Ok(())
        }

        fn ensure_account_persists(&mut self, addr: &Address) -> Result<()> {
            let acc = self.db.account_mut(addr);
            if acc.nonce == 0 {
                acc.nonce = 1;
            }
            Ok(())
        }

        fn iter_storage_asc(&mut self, addr: &Address, from: B256) -> Result<Vec<(B256, B256)>> {
            iter_storage_asc_impl(self, addr, from)
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{InMemoryStateAdapter, InMemoryStateDb};
    use alloy_primitives::U256;
    use alloy_primitives::b256;

    // ─── Primitives ──────────────────────────────────────────────────

    #[test]
    fn entity_address_truncates_to_first_20_bytes() {
        let key = b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        assert_eq!(entity_address(key).as_slice(), &key.0[..20]);
    }

    #[test]
    fn pair_address_separator_prevents_prefix_collision() {
        assert_ne!(pair_address(b"ab", b"c"), pair_address(b"a", b"bc"));
    }

    #[test]
    fn bitmap_serialization_is_deterministic() {
        // Same set inserted in different orders → identical bytes.
        let ids = [3u64, 1, 2, 42, 1_000_001, 1_000_000];
        let mut a = Bitmap::new();
        let mut b = Bitmap::new();
        for id in ids {
            a.insert(id);
        }
        for id in ids.iter().rev() {
            b.insert(*id);
        }
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn entity_rlp_roundtrip_via_code() {
        let original = EntityRlp {
            payload: b"hello".to_vec(),
            creator: Address::repeat_byte(0xaa),
            created_at_block: 1234,
            owner: Address::repeat_byte(0xbb),
            expires_at: 99_999,
            content_type: b"application/json".to_vec(),
            key: b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"),
            attributes: vec![
                Attribute {
                    key: b"title".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"the answer".to_vec(),
                },
                Attribute {
                    key: b"priority".to_vec(),
                    value_type: ATTR_UINT,
                    value: U256::from(42).to_be_bytes::<32>().to_vec(),
                },
                Attribute {
                    key: b"replyTo".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: vec![0xab; 32],
                },
            ],
            last_modified_at_block: 1234,
        };
        let code = original.encode_as_code();
        assert_eq!(code[0], ENTITY_CODE_PREFIX);
        assert_eq!(
            EntityRlp::decode_from_code(&code).expect("decode"),
            original
        );
    }

    #[test]
    fn entity_rlp_decode_requires_fe_prefix() {
        let entity = EntityRlp {
            payload: vec![],
            creator: Address::ZERO,
            created_at_block: 0,
            owner: Address::ZERO,
            expires_at: 0,
            content_type: vec![],
            key: B256::ZERO,
            attributes: vec![],
            last_modified_at_block: 0,
        };
        let mut bad = entity.encode_as_code();
        bad[0] = 0x00;
        assert!(EntityRlp::decode_from_code(&bad).is_err());
    }

    // ─── Op handlers (against InMemoryStateAdapter) ───────────────────────

    fn fresh_db() -> InMemoryStateDb {
        InMemoryStateDb::default()
    }

    fn alice() -> Address {
        Address::repeat_byte(0xaa)
    }
    fn bob() -> Address {
        Address::repeat_byte(0xbb)
    }
    fn entity_key_n(n: u8) -> B256 {
        B256::from([n; 32])
    }

    #[track_caller]
    fn read_bitmap(db: &InMemoryStateDb, annot_key: &[u8], annot_val: &[u8]) -> Bitmap {
        let addr = pair_address(annot_key, annot_val);
        let code = db
            .account(&addr)
            .map(|a| a.code.clone())
            .unwrap_or_default();
        if code.is_empty() {
            Bitmap::new()
        } else {
            Bitmap::from_bytes(&code).expect("decode bitmap")
        }
    }

    #[test]
    fn create_writes_entity_and_all_bitmaps() {
        let mut db = fresh_db();
        let key = entity_key_n(0x42);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                b"hello".to_vec(),
                b"text/plain".to_vec(),
                vec![
                    Attribute {
                        key: b"tag".to_vec(),
                        value_type: ATTR_STRING,
                        value: b"music".to_vec(),
                    },
                    Attribute {
                        key: b"score".to_vec(),
                        value_type: ATTR_UINT,
                        value: U256::from(7).to_be_bytes::<32>().to_vec(),
                    },
                ],
            )
            .expect("create");
        }

        // Entity-account code written; round-trips through EntityRlp.
        let entity_addr = entity_address(key);
        let code = db.account(&entity_addr).expect("entity acc").code.clone();
        let entity = EntityRlp::decode_from_code(&code).expect("decode");
        assert_eq!(entity.owner, alice());
        assert_eq!(entity.creator, alice());
        assert_eq!(entity.expires_at, 100);
        assert_eq!(entity.created_at_block, 10);

        // System counter advanced to 1; this entity got id=0.
        let count = db
            .account(&SYSTEM_ACCOUNT_ADDRESS)
            .expect("system acc")
            .storage
            .get(&slot_entity_count())
            .copied()
            .unwrap_or_default();
        assert_eq!(storage_to_u64(count), 1);

        // All built-ins + user annotations contain entity_id=0.
        assert!(read_bitmap(&db, ANNOT_ALL, b"").contains(0));
        assert!(read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
        assert!(read_bitmap(&db, ANNOT_EXPIRATION, &100u64.to_be_bytes()).contains(0));
        assert!(read_bitmap(&db, ANNOT_CONTENT_TYPE, b"text/plain").contains(0));
        assert!(read_bitmap(&db, b"tag", b"music").contains(0));
        assert!(read_bitmap(&db, b"score", &U256::from(7).to_be_bytes::<32>()).contains(0));
    }

    #[test]
    fn transfer_moves_owner_bitmap() {
        let mut db = fresh_db();
        let key = entity_key_n(1);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
            transfer(&mut state, key, 20, bob()).unwrap();
        }
        assert!(!read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
        assert!(read_bitmap(&db, ANNOT_OWNER, bob().as_slice()).contains(0));

        // Entity RLP reflects the new owner.
        let entity_addr = entity_address(key);
        let entity = EntityRlp::decode_from_code(&db.account(&entity_addr).unwrap().code).unwrap();
        assert_eq!(entity.owner, bob());
    }

    #[test]
    fn extend_moves_expiration_bitmap() {
        let mut db = fresh_db();
        let key = entity_key_n(2);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
            extend(&mut state, key, 20, 500).unwrap();
        }
        assert!(!read_bitmap(&db, ANNOT_EXPIRATION, &100u64.to_be_bytes()).contains(0));
        assert!(read_bitmap(&db, ANNOT_EXPIRATION, &500u64.to_be_bytes()).contains(0));

        let entity_addr = entity_address(key);
        let entity = EntityRlp::decode_from_code(&db.account(&entity_addr).unwrap().code).unwrap();
        assert_eq!(entity.expires_at, 500);
    }

    #[test]
    fn update_diffs_only_changed_annotations() {
        let mut db = fresh_db();
        let key = entity_key_n(3);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"tag".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"a".to_vec(),
                }],
            )
            .unwrap();
            // Change the tag value; keep content type the same.
            update(
                &mut state,
                key,
                20,
                vec![0xff],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"tag".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"b".to_vec(),
                }],
            )
            .unwrap();
        }
        // tag=a bitmap loses the entity, tag=b gains it.
        assert!(!read_bitmap(&db, b"tag", b"a").contains(0));
        assert!(read_bitmap(&db, b"tag", b"b").contains(0));
        // content type unchanged → bitmap still contains it.
        assert!(read_bitmap(&db, ANNOT_CONTENT_TYPE, b"text/plain").contains(0));
        // Owner/expiration untouched.
        assert!(read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
    }

    #[test]
    fn delete_clears_bitmaps_and_tombstones_account() {
        let mut db = fresh_db();
        let key = entity_key_n(4);
        let entity_addr = entity_address(key);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
            delete(&mut state, key).unwrap();
        }
        // Bitmaps drop the entity.
        assert!(!read_bitmap(&db, ANNOT_ALL, b"").contains(0));
        assert!(!read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));

        // ID-map slots cleared.
        let count_slot = slot_entity_count();
        let _ = count_slot; // counter NOT decremented — only ID maps cleared.
        let id_to_addr_slot = slot_id_to_addr(0);
        assert_eq!(
            db.account(&SYSTEM_ACCOUNT_ADDRESS)
                .unwrap()
                .storage
                .get(&id_to_addr_slot)
                .copied()
                .unwrap_or_default(),
            B256::ZERO
        );

        // Entity account tombstoned: code empty, nonce=1.
        let acc = db.account(&entity_addr).expect("entity acc still exists");
        assert!(acc.code.is_empty());
        assert_eq!(acc.nonce, 1);
    }

    // ─── Tier-2 index (storage-slot based) ──────────────────────────

    fn str_slot(db: &InMemoryStateDb, attr_key: &[u8], prefix: &[u8], chunk: B256) -> B256 {
        let addr = str_level_address(attr_key, prefix);
        db.account(&addr)
            .and_then(|a| a.storage.get(&chunk).copied())
            .unwrap_or(B256::ZERO)
    }

    #[test]
    fn insert_into_indexes_writes_tier2_on_first_entity() {
        let mut db = fresh_db();
        let val = b"hello".to_vec();
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            insert_into_indexes(&mut state, b"tag", &val, 0, Some(AnnotMode::Int)).unwrap();
        }
        let header_addr = btree_header_address(b"tag");
        // Header must have BTREE_MAGIC at byte 16.
        let slot0 = db.account(&header_addr)
            .and_then(|a| a.storage.get(&B256::ZERO).copied())
            .unwrap_or(B256::ZERO);
        assert_eq!(slot0.0[16], BTREE_MAGIC);
        // iter_storage_asc must return the inserted value.
        let mut state = InMemoryStateAdapter::new(&mut db);
        let entries = state.iter_storage_asc(&header_addr, B256::ZERO).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, annot_val_to_slot(b"hello"));
        assert_eq!(entries[0].1, slot_presence(5));

        // Second insert of same value — bitmap non-empty, tier2 not called again.
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            insert_into_indexes(&mut state, b"tag", &val, 1, Some(AnnotMode::Int)).unwrap();
        }
        let mut state = InMemoryStateAdapter::new(&mut db);
        let entries = state.iter_storage_asc(&header_addr, B256::ZERO).unwrap();
        assert_eq!(entries.len(), 1, "still one distinct value");
    }

    #[test]
    fn remove_from_indexes_removes_tier2_on_last_entity() {
        let mut db = fresh_db();
        let val = b"hello".to_vec();
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            insert_into_indexes(&mut state, b"tag", &val, 0, Some(AnnotMode::Int)).unwrap();
            insert_into_indexes(&mut state, b"tag", &val, 1, Some(AnnotMode::Int)).unwrap();
        }
        let header_addr = btree_header_address(b"tag");

        // Remove first entity — bitmap still has entity 1, tier2 not triggered.
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            remove_from_indexes(&mut state, b"tag", &val, 0, Some(AnnotMode::Int)).unwrap();
        }
        let mut state = InMemoryStateAdapter::new(&mut db);
        let entries = state.iter_storage_asc(&header_addr, B256::ZERO).unwrap();
        assert_eq!(entries.len(), 1, "value should survive while entity 1 remains");

        // Remove last entity — lazy deleted, iter excludes it.
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            remove_from_indexes(&mut state, b"tag", &val, 1, Some(AnnotMode::Int)).unwrap();
        }
        let mut state = InMemoryStateAdapter::new(&mut db);
        let entries = state.iter_storage_asc(&header_addr, B256::ZERO).unwrap();
        assert_eq!(entries.len(), 0, "value should be excluded after last entity removed");
    }

    #[test]
    fn tier2_str_insert_sets_level0_slot() {
        let mut db = fresh_db();
        let val = b"image/png".to_vec(); // 9 bytes, single chunk
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            insert_into_indexes(&mut state, b"ct", &val, 0, Some(AnnotMode::Str)).unwrap();
        }
        let chunk = value_chunks(b"image/png")[0];
        let slot_val = str_slot(&db, b"ct", b"", chunk);
        assert_eq!(slot_val, slot_presence(9));
    }

    #[test]
    fn next_prefix_bound_slot_increments_correctly() {
        let bound = next_prefix_bound_slot(b"ab");
        // "ab" right-padded is [0x61, 0x62, 0, ...]. Increment at pos 1:
        // → [0x61, 0x63, 0, ...].
        let mut expected = [0u8; 32];
        expected[0] = 0x61;
        expected[1] = 0x63;
        assert_eq!(bound, Some(B256::from(expected)));
    }

    #[test]
    fn iter_storage_asc_in_memory_sorted() {
        let mut db = fresh_db();
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            insert_into_indexes(&mut state, b"k", b"c", 0, Some(AnnotMode::Int)).unwrap();
            insert_into_indexes(&mut state, b"k", b"a", 1, Some(AnnotMode::Int)).unwrap();
            insert_into_indexes(&mut state, b"k", b"b", 2, Some(AnnotMode::Int)).unwrap();
        }
        let mut state = InMemoryStateAdapter::new(&mut db);
        let header_addr = btree_header_address(b"k");
        let entries = state.iter_storage_asc(&header_addr, B256::ZERO).unwrap();
        let keys: Vec<B256> = entries.iter().map(|(k, _)| *k).collect();
        assert!(keys.windows(2).all(|w| w[0] <= w[1]), "must be sorted");
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn expire_has_same_state_path_as_delete() {
        let mut db_a = fresh_db();
        let mut db_b = fresh_db();
        let key = entity_key_n(5);
        for db in [&mut db_a, &mut db_b] {
            let mut state = InMemoryStateAdapter::new(db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
        }
        {
            let mut state = InMemoryStateAdapter::new(&mut db_a);
            delete(&mut state, key).unwrap();
        }
        {
            let mut state = InMemoryStateAdapter::new(&mut db_b);
            expire(&mut state, key).unwrap();
        }
        // Equal: both paths produce the same account map.
        for addr in [
            entity_address(key),
            SYSTEM_ACCOUNT_ADDRESS,
            pair_address(ANNOT_ALL, b""),
            pair_address(ANNOT_OWNER, alice().as_slice()),
        ] {
            assert_eq!(
                db_a.account(&addr),
                db_b.account(&addr),
                "mismatch at {addr}"
            );
        }
    }

    // ─── Tier-2 skip-list (issue #96) ────────────────────────────────

    #[track_caller]
    fn index_account_present(db: &InMemoryStateDb, attr_key: &[u8]) -> bool {
        db.account(&btree_header_address(attr_key)).is_some()
            || db.account(&str_level_address(attr_key, b"")).is_some()
    }

    #[test]
    fn create_skips_tier2_index_for_skip_listed_built_ins() {
        let mut db = fresh_db();
        let key = entity_key_n(1);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
        }
        for k in [ANNOT_ALL, ANNOT_CREATOR, ANNOT_OWNER, ANNOT_KEY] {
            assert!(
                !index_account_present(&db, k),
                "index_address({:?}) should not exist for skip-listed built-in",
                std::str::from_utf8(k).unwrap()
            );
        }
    }

    #[test]
    fn create_keeps_tier2_index_for_tier2_built_ins() {
        let mut db = fresh_db();
        let key = entity_key_n(2);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
        }
        for k in [ANNOT_CREATED_AT_BLOCK, ANNOT_EXPIRATION, ANNOT_CONTENT_TYPE] {
            assert!(
                index_account_present(&db, k),
                "index_address({:?}) should exist for tier-2 built-in",
                std::str::from_utf8(k).unwrap()
            );
        }
    }

    #[test]
    fn create_skips_tier2_index_for_user_entity_key_attribute() {
        let mut db = fresh_db();
        let key = entity_key_n(3);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"ref".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: entity_key_n(99).as_slice().to_vec(),
                }],
            )
            .unwrap();
        }
        assert!(!index_account_present(&db, b"ref"));
    }

    #[test]
    fn create_keeps_tier2_index_for_user_uint_and_string_attributes() {
        let mut db = fresh_db();
        let key = entity_key_n(4);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![
                    Attribute {
                        key: b"tag".to_vec(),
                        value_type: ATTR_STRING,
                        value: b"music".to_vec(),
                    },
                    Attribute {
                        key: b"score".to_vec(),
                        value_type: ATTR_UINT,
                        value: U256::from(7).to_be_bytes::<32>().to_vec(),
                    },
                ],
            )
            .unwrap();
        }
        assert!(index_account_present(&db, b"tag"));
        assert!(index_account_present(&db, b"score"));
    }

    #[test]
    fn eq_lookup_via_tier1_still_works_for_skip_listed_attrs() {
        // Tier-1 pair bitmaps are unaffected by the skip list.
        let mut db = fresh_db();
        let key = entity_key_n(5);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"ref".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: entity_key_n(99).as_slice().to_vec(),
                }],
            )
            .unwrap();
        }
        assert!(read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
        assert!(read_bitmap(&db, ANNOT_KEY, key.as_slice()).contains(0));
        assert!(read_bitmap(&db, b"ref", entity_key_n(99).as_slice()).contains(0));
    }

    #[test]
    fn delete_does_not_touch_absent_tier2_indexes() {
        // delete must not error or allocate index accounts for
        // skip-listed annotations, even though the same code path
        // tombstones the kept ones.
        let mut db = fresh_db();
        let key = entity_key_n(6);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"ref".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: entity_key_n(99).as_slice().to_vec(),
                }],
            )
            .unwrap();
            delete(&mut state, key).unwrap();
        }
        for k in [
            ANNOT_ALL,
            ANNOT_CREATOR,
            ANNOT_OWNER,
            ANNOT_KEY,
            b"ref" as &[u8],
        ] {
            assert!(
                !index_account_present(&db, k),
                "skip-listed index_address({:?}) should still not exist after delete",
                std::str::from_utf8(k).unwrap()
            );
        }
    }

    #[test]
    fn transfer_does_not_touch_owner_tier2_index() {
        // $owner is skip-listed; transfer moves the tier-1 bitmap but
        // never allocates an $owner index account.
        let mut db = fresh_db();
        let key = entity_key_n(7);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
        }
        assert!(!index_account_present(&db, ANNOT_OWNER));
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            transfer(&mut state, key, 11, bob()).unwrap();
        }
        assert!(!index_account_present(&db, ANNOT_OWNER));
        // Tier-1 bitmap moved.
        assert!(!read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
        assert!(read_bitmap(&db, ANNOT_OWNER, bob().as_slice()).contains(0));
    }
}
