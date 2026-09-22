//! The **backing-store seam** — Arkiv's state as a commit/branch database.
//!
//! This is a Rust rendering of `golem-db-api.md`. It is deliberately a
//! *transcription*, not an adaptation: the real store must drop in behind this
//! trait unchanged, so where the spec and Arkiv's existing
//! [`statemanager`](crate::statemanager) traits disagree, this module follows
//! the spec. The conformance suite ([`conformance`]) is the contract both the
//! reference implementation and the real store are held to.
//!
//! # Shape
//!
//! Two layers. A durable, linear **commit** history ([`CommitId`], +1 per
//! commit, never forking), and volatile in-memory **branches** ([`BranchId`])
//! doing work on top of it. Writes target a branch; queries target a commit.
//!
//! ```text
//! begin()      ── root branch over the canonical head
//!   fork()     ── child frame (a tx)
//!     fork()   ── grandchild frame (an atomic op batch)
//!     merge()  ── or discard(), on failure
//!   merge()
//! commit()     ── seal: assigns head+1, durable
//! ```
//!
//! # Deviations from `golem-db-api.md`
//!
//! Everything marked **[ext]** is an addition this seam requires and the spec
//! does not yet provide; see `golem-db-api-reth-requirements.md` in
//! `arkiv-architecture-review` for why each one is needed. They are grouped in
//! [`StoreExt`] rather than [`Store`] so the gap between "what the spec says"
//! and "what a reth host needs" stays legible, and shrinks visibly as the spec
//! catches up.
//!
//! Two smaller resolutions taken here because the spec leaves them open:
//!
//! - **Record keys are a fixed 32 bytes** ([`RecordKey`]). The spec's
//!   separator-free `record_key ‖ cell_name` composition requires fixed width,
//!   but Arkiv mixes 32-byte entity addresses with 20-byte user addresses. A
//!   20-byte address is left-padded into the 32-byte key, ABI style, keeping
//!   composition unambiguous with one key width.
//! - **Cell names are `str`**, with the one rule the spec does pin already:
//!   a caller-supplied name may not start with `#` (reserved for store-internal
//!   meta entries). See [`validate_cell_name`]. The full grammar is still open
//!   upstream; when it lands it belongs in that function and nowhere else.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Debug;

// ---------------------------------------------------------------------------
// Handles and scalars
// ---------------------------------------------------------------------------

/// A point in the canonical history. `0` is genesis (empty state); every
/// [`Store::commit`] assigns `head + 1`, gapless. The lineage never forks —
/// competing candidates are [`BranchId`]s until one commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CommitId(pub u64);

impl CommitId {
    /// The empty state, before any commit.
    pub const GENESIS: Self = Self(0);
}

/// A handle to one open branch. Store-assigned, monotonic per instance run,
/// **never reused**: consumed by `merge` / `discard` / `commit`, and any later
/// use is [`StoreError::HandleInvalid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchId(pub u64);

/// A branch's mutation counter: starts at 0, +1 per absorbed mutation batch
/// (a direct write call, or a child's `merge`).
///
/// The three number spaces — [`CommitId`], [`BranchId`], `BranchVersion` — are
/// independent; `c7`, `b7` and `v7` may all coexist and mean nothing to each
/// other. Anything human-facing must say which space a number belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BranchVersion(pub u64);

/// A record's mutation counter, bumped whenever *any* cell of the record
/// changes. Versions start at 1; `0` is therefore a guard that can never match.
///
/// Coordination metadata, not content: stored and replicated, but **excluded
/// from [`Store::branch_hash`]**, so a write that leaves a record logically
/// unchanged never moves the digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RecordVersion(pub u64);

/// A cap on what one call may spend. Exceeding it aborts with
/// [`StoreError::OutOfBudget`] and **no partial results**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Budget(pub u64);

/// Cost units spent by one call, priced against a [`ScheduleVersion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CostUnits(pub u64);

/// Which pricing table a [`Receipt`] was computed under. Monotonic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ScheduleVersion(pub u64);

/// What every data-plane call reports: what it cost, and under which schedule.
///
/// Errors carry one too — cost spent before an abort is still spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Receipt {
    pub cost: CostUnits,
    pub schedule: ScheduleVersion,
}

/// A data-plane result: the value, plus what producing it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metered<T> {
    pub value: T,
    pub receipt: Receipt,
}

impl<T> Metered<T> {
    pub const fn new(value: T, receipt: Receipt) -> Self {
        Self { value, receipt }
    }

    /// Drop the receipt. For call sites that genuinely do not meter — tests,
    /// and host paths where the cost is accounted elsewhere.
    pub fn into_value(self) -> T {
        self.value
    }
}

// ---------------------------------------------------------------------------
// Records and cells
// ---------------------------------------------------------------------------

/// A record's unique identity: a fixed 32 bytes.
///
/// Fixed width is what makes the store's `record_key ‖ cell_name` layout
/// unambiguous without a separator (cell-name grammars may contain any
/// candidate separator character). Narrower Arkiv keys — a 20-byte
/// [`UserAddress`](crate::primitives::UserAddress) — are left-padded in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RecordKey(pub [u8; 32]);

impl RecordKey {
    /// An [`EntityAddress`](crate::primitives::EntityAddress) is already the
    /// full key width.
    pub const fn from_entity(address: [u8; 32]) -> Self {
        Self(address)
    }

    /// Left-pad a 20-byte address into the 32-byte key space, ABI style.
    pub const fn from_address(address: [u8; 20]) -> Self {
        let mut key = [0u8; 32];
        let mut i = 0;
        while i < 20 {
            key[12 + i] = address[i];
            i += 1;
        }
        Self(key)
    }

    /// The inverse of [`from_address`](Self::from_address). `None` if the
    /// leading 12 bytes are not zero, i.e. this key is not a padded address.
    pub const fn as_address(&self) -> Option<[u8; 20]> {
        let mut i = 0;
        while i < 12 {
            if self.0[i] != 0 {
                return None;
            }
            i += 1;
        }
        let mut address = [0u8; 20];
        let mut j = 0;
        while j < 20 {
            address[j] = self.0[12 + j];
            j += 1;
        }
        Some(address)
    }
}

/// Whether a cell is indexed.
///
/// The distinction is the store's whole cost model: an **attribute** is what
/// `query` and `count` filter and sort on, and its write price includes the
/// index maintenance it triggers; a **field** is invisible to queries and costs
/// only the bytes moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CellKind {
    /// Indexed on first use; queryable.
    Attribute,
    /// Never indexed; point-read only via [`Store::get`].
    Field,
}

/// A value's type, as the tag byte's low 7 bits.
///
/// `1..=31` are core types fixed by the spec; `32..=127` are registered per
/// deployment (compile-time or instance config — never a runtime call). `0` is
/// reserved and always invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeId(pub u8);

impl TypeId {
    pub const BOOL: Self = Self(1);
    pub const I32: Self = Self(2);
    pub const U256: Self = Self(3);
    pub const DEC: Self = Self(4);
    pub const BYTES32: Self = Self(5);
    /// Variable width, **field-only** — an attribute of this type is
    /// [`StoreError::InvalidArgument`].
    pub const BYTES: Self = Self(6);
    pub const STR: Self = Self(7);
    pub const U64: Self = Self(8);

    /// The Arkiv profile's registered types.
    pub const ADDR: Self = Self(32);
    pub const KEY: Self = Self(33);

    /// `0` is reserved; the tag byte's kind bit caps ids at 127.
    pub const fn is_valid(self) -> bool {
        self.0 >= 1 && self.0 <= 127
    }

    /// Whether this type may be used as an indexed [`CellKind::Attribute`].
    /// Only `bytes` is field-only among the core types.
    pub const fn indexable(self) -> bool {
        self.0 != Self::BYTES.0
    }

    /// What index operations this type supports. Drives the
    /// [`StoreError::InvalidQuery`] guard on range and prefix predicates.
    pub const fn index_class(self) -> IndexClass {
        match self.0 {
            1 | 5 | 32 | 33 => IndexClass::Eq,
            2 | 3 | 4 | 8 => IndexClass::EqRange,
            7 => IndexClass::EqPrefix,
            6 => IndexClass::None,
            // Registered types declare their own; unknown ids are rejected at
            // write time, so equality is the safe floor here.
            _ => IndexClass::Eq,
        }
    }
}

/// What a type's index supports. A range or prefix predicate against a type
/// lacking it is [`StoreError::InvalidQuery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexClass {
    /// Field-only: not indexable at all.
    None,
    Eq,
    EqRange,
    EqPrefix,
}

impl IndexClass {
    pub const fn supports_range(self) -> bool {
        matches!(self, Self::EqRange)
    }

    pub const fn supports_prefix(self) -> bool {
        matches!(self, Self::EqPrefix)
    }
}

/// One stored value: its kind, its type, and its canonical bytes.
///
/// **Typing is per write.** A later write to the same name may change both kind
/// and type; the index entry simply moves. Nothing anywhere declares a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub kind: CellKind,
    pub type_id: TypeId,
    /// Canonical encoding for `type_id` — one valid byte form per value.
    pub value: Vec<u8>,
}

impl Cell {
    pub fn attribute(type_id: TypeId, value: Vec<u8>) -> Self {
        Self {
            kind: CellKind::Attribute,
            type_id,
            value,
        }
    }

    pub fn field(type_id: TypeId, value: Vec<u8>) -> Self {
        Self {
            kind: CellKind::Field,
            type_id,
            value,
        }
    }

    /// The single self-describing tag byte stored ahead of the value:
    /// `bit 7` = kind (0 attribute, 1 field), `bits 0–6` = [`TypeId`].
    ///
    /// The tag travels **with the value and under the commitment**, so a proof
    /// attests a *typed* value and stored cells need no registry to interpret.
    pub const fn tag(&self) -> u8 {
        let kind_bit = match self.kind {
            CellKind::Attribute => 0x00,
            CellKind::Field => 0x80,
        };
        kind_bit | self.type_id.0
    }

    /// Split a tag byte back into kind and type. `None` if the type id is `0`.
    pub const fn split_tag(tag: u8) -> Option<(CellKind, TypeId)> {
        let type_id = TypeId(tag & 0x7F);
        if !type_id.is_valid() {
            return None;
        }
        let kind = if tag & 0x80 == 0 {
            CellKind::Attribute
        } else {
            CellKind::Field
        };
        Some((kind, type_id))
    }

    /// Reject a cell the store must not store: an unknown type, or an indexed
    /// cell of a field-only type.
    pub const fn validate(&self) -> Result<(), StoreError> {
        if !self.type_id.is_valid() {
            return Err(StoreError::InvalidArgument);
        }
        if matches!(self.kind, CellKind::Attribute) && !self.type_id.indexable() {
            return Err(StoreError::InvalidArgument);
        }
        Ok(())
    }
}

/// A cell's name within its record's flat namespace.
pub type CellName = String;

/// The one naming rule the spec pins today: `#` is reserved for store-internal
/// meta entries (`#version`) and is rejected in any caller-supplied cell map.
///
/// The full grammar — character set, length cap, structural rules — is still
/// open upstream. When it lands it belongs here and nowhere else.
pub fn validate_cell_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() || name.starts_with('#') {
        return Err(StoreError::InvalidArgument);
    }
    Ok(())
}

/// A record as read back: its key, its version, and its cells.
///
/// Cells are self-describing on the way out, mirroring the stored tag byte, so
/// a reader never consults a schema. Ordering is ascending by name, so equal
/// records compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: RecordKey,
    pub version: RecordVersion,
    pub cells: Vec<(CellName, Cell)>,
}

impl Record {
    /// The named cell, if present.
    pub fn cell(&self, name: &str) -> Option<&Cell> {
        self.cells
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, cell)| cell)
    }
}

/// One entry in a [`Store::patch`] cell map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellChange {
    Set(Cell),
    Remove,
}

// ---------------------------------------------------------------------------
// Targets, queries
// ---------------------------------------------------------------------------

/// What a read is answered from.
///
/// Point reads ([`Store::get`]) accept either. Queries accept **only** a
/// commit: `query` and `count` never see a branch's uncommitted writes. That
/// is a spec-level constraint, and it holds for Arkiv — the executor reads
/// entities by key, and `arkiv_query` runs against a committed snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadTarget {
    /// Reads through the branch's diff stack, then its origin.
    Branch(BranchId),
    /// A historical point read, within the retention window.
    Commit(CommitId),
}

/// How a predicate compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
    /// Raw byte prefix, no normalization. `str` only.
    Prefix,
}

impl CompareOp {
    /// Whether `class` admits this operation.
    pub const fn permitted_by(self, class: IndexClass) -> bool {
        match self {
            Self::Eq => !matches!(class, IndexClass::None),
            Self::Lt | Self::Lte | Self::Gt | Self::Gte => class.supports_range(),
            Self::Prefix => class.supports_prefix(),
        }
    }
}

/// One comparison against one attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Predicate {
    pub cell: CellName,
    pub op: CompareOp,
    pub type_id: TypeId,
    pub value: Vec<u8>,
    /// Complement: matches records the predicate does *not*.
    pub negated: bool,
}

/// An AND-group: predicates intersect **in submitted order**.
///
/// Order is significant and never rearranged. Intersection cost tracks operand
/// sizes, so putting the most selective predicate first is the caller's main
/// optimization lever — and since [`Budget`] thresholds cost, it can decide
/// whether a query completes at all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AndGroup(pub Vec<Predicate>);

/// An ordered DNF: OR of AND-groups. The store evaluates groups in submitted
/// order and unions the results; a record matching several groups is returned
/// **once**.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filter(pub Vec<AndGroup>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

/// One sort key. Absent, and as the tie-break under a sort, results are in
/// ascending [`RecordKey`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sort {
    pub cell: CellName,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Page {
    pub offset: u64,
    pub limit: u64,
}

/// A filtered, sorted, paged read against committed state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    pub filter: Filter,
    pub sort: Option<Sort>,
    pub page: Page,
    /// Cell names to return; absent = the full record.
    pub projection: Option<Vec<CellName>>,
    /// Ask for the full match count. Suppresses page early-exit: every group
    /// runs to completion.
    pub total_matched: bool,
}

/// What [`Store::query`] returns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryResult {
    pub records: Vec<Record>,
    /// Present only when [`Query::total_matched`] was set.
    pub total_matched: Option<u64>,
}

/// A branch's origin, fixed at creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A root branch, over an immutable commit. May [`Store::commit`].
    Commit(CommitId),
    /// A child branch, over its parent at the parent's version when forked.
    /// May only `merge` or `discard`.
    Branch(BranchId, BranchVersion),
}

/// What [`Store::branch_info`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchInfo {
    pub origin: Origin,
    pub version: BranchVersion,
    /// Layers between this branch and its grounding commit. `0` for a root.
    pub depth: u32,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The shared error set. Data-plane calls report cost spent alongside these;
/// the cost rides in [`Metered`] on success and is reported out-of-band on
/// failure, so this stays a plain enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    NotFound,
    AlreadyExists,
    /// The `key` argument disagreed with the branch lineage's key-assignment
    /// mode.
    KeyModeMismatch,
    /// A range or prefix op against a type whose [`IndexClass`] lacks it.
    InvalidQuery,
    InvalidArgument,
    /// A structural cap — group count, predicates per group, nesting depth.
    LimitExceeded,
    /// [`Budget`] exhausted. No partial results.
    OutOfBudget {
        spent: CostUnits,
    },
    /// A consumed or unknown [`BranchId`].
    HandleInvalid,
    /// An optimistic-concurrency guard failed, or `commit`'s origin is no
    /// longer head, or `merge`'s parent moved.
    ///
    /// In blockchain mode the OCC cases are unreachable — a single proposer
    /// executing serially never supplies a guard. The `commit` case is the
    /// live one, and a host should treat it as fatal: a second block reaching
    /// a committed height means the store and the ledger have diverged.
    Conflict,
    Internal,
}

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// The backing store, as `golem-db-api.md` specifies it.
///
/// Implementations: the in-memory reference (`arkiv-store-mem`), and the real
/// store. Both are held to [`conformance`].
pub trait Store {
    // -- commits and branches (unmetered) ----------------------------------

    /// The current canonical head.
    fn head(&self) -> CommitId;

    /// Open a root branch over canonical state. `at` defaults to head and must
    /// lie within the retention window. Root branches may [`commit`](Self::commit).
    ///
    /// Any number may be open concurrently over the same origin — which is what
    /// lets a host validate competing payloads at one height.
    fn begin(&mut self, at: Option<CommitId>) -> Result<BranchId, StoreError>;

    /// An O(1) child branch, snapshotting `parent` at its current version. The
    /// parent may advance afterwards without affecting the child. Child
    /// branches may only `merge` or `discard`.
    fn fork(&mut self, parent: BranchId) -> Result<BranchId, StoreError>;

    /// Fold `child`'s diff into its parent, consuming the handle. The parent's
    /// version must still equal the child's fork-time version, else
    /// [`StoreError::Conflict`]. Returns the parent's new version.
    fn merge(&mut self, child: BranchId) -> Result<BranchVersion, StoreError>;

    /// Drop a branch and its open descendants; the parent is untouched.
    /// Receipts already returned stay valid — cost is a return value, never
    /// state.
    fn discard(&mut self, branch: BranchId) -> Result<(), StoreError>;

    /// Seal a root branch: assigns `head + 1`, writes history, persists
    /// durably, consumes the handle. The origin commit must still be head,
    /// else [`StoreError::Conflict`].
    fn commit(&mut self, root: BranchId) -> Result<CommitId, StoreError>;

    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo, StoreError>;

    // -- CRUD (metered) ----------------------------------------------------

    /// Insert a new record. Collision is [`StoreError::AlreadyExists`].
    ///
    /// Besides `cells`, the store writes the record's `#version` meta entry at
    /// `1` — its presence *is* record existence.
    fn create(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        cells: Vec<(CellName, Cell)>,
        budget: Option<Budget>,
    ) -> Result<Metered<RecordKey>, StoreError>;

    /// Point read by key, through a branch's diff stack or against a commit.
    fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        budget: Option<Budget>,
    ) -> Result<Metered<Option<Record>>, StoreError>;

    /// Partial mutation. Names absent from `changes` are untouched; the last
    /// cell of a record may not be removed. Returns the new record version.
    fn patch(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
        budget: Option<Budget>,
    ) -> Result<Metered<RecordVersion>, StoreError>;

    fn delete(
        &mut self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        budget: Option<Budget>,
    ) -> Result<Metered<()>, StoreError>;

    // -- query (metered, commit-targeted) ----------------------------------

    /// Filtered, sorted, paged read against committed state. `at` defaults to
    /// head and must lie within retention.
    fn query(
        &self,
        at: Option<CommitId>,
        query: &Query,
        budget: Option<Budget>,
    ) -> Result<Metered<QueryResult>, StoreError>;

    /// Count matches without materializing records.
    fn count(
        &self,
        at: Option<CommitId>,
        filter: &Filter,
        budget: Option<Budget>,
    ) -> Result<Metered<u64>, StoreError>;

    // -- introspection (unmetered) -----------------------------------------

    /// The branch's incrementally maintained content digest — the value that
    /// becomes the block's state root. `~O(1)`.
    ///
    /// A pure function of logical record content: `#`-prefixed meta entries,
    /// [`RecordVersion`] included, are **excluded**.
    fn branch_hash(&self, branch: BranchId) -> Result<[u8; 32], StoreError>;
}

/// Host-facing additions this seam needs and `golem-db-api.md` does not yet
/// provide. Each is **[ext]**; see `golem-db-api-reth-requirements.md`.
///
/// Kept separate from [`Store`] so the gap between the spec and what a reth
/// host requires stays legible — and shrinks visibly as the spec catches up.
pub trait StoreExt: Store {
    /// **[ext R1]** Seal a root branch, tagging the commit with an opaque
    /// 32-byte host identifier — for Arkiv, the block hash.
    ///
    /// reth addresses state by block hash everywhere (`state_by_block_hash`,
    /// forkchoice, the engine tree's keying), which a `u64` [`CommitId`]
    /// cannot express. The store attaches no meaning to the tag; it only has
    /// to keep it consistent with retention trimming, which is why this lives
    /// store-side rather than in a host map that could disagree after a crash.
    fn commit_tagged(&mut self, root: BranchId, tag: [u8; 32]) -> Result<CommitId, StoreError>;

    /// **[ext R1]** Resolve a tag back to its commit, if still retained.
    fn commit_by_tag(&self, tag: [u8; 32]) -> Result<Option<CommitId>, StoreError>;

    /// **[ext R2]** What one commit changed, as `(key, before, after)`.
    ///
    /// Three consumers: reth's `ChangeSetReader` / `StorageChangeSetReader`,
    /// which any provider the engine tree accepts must implement; txpool
    /// maintenance, which needs changed accounts on every block and reorg to
    /// evict stale-nonce and demote unaffordable transactions; and verifying
    /// a rewind did what it claimed.
    fn changes(&self, commit: CommitId) -> Result<Vec<RecordChange>, StoreError>;

    /// **[ext R3]** Apply many writes under one call and one receipt.
    ///
    /// A block is hundreds of individual calls and a genesis seed is
    /// thousands. Tolerable in-process; fatal across a process boundary, which
    /// is exactly where the A/B equivalence harness lives.
    fn apply(
        &mut self,
        branch: BranchId,
        ops: Vec<WriteOp>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<WriteOutcome>>, StoreError>;

    /// **[ext R4]** A committed digest, after the branch handle is gone.
    ///
    /// [`Store::branch_hash`] takes a branch, and `commit` consumes it — so
    /// there is otherwise no way to ask what commit `c` hashed to. Needed to
    /// validate a peer's block and to re-derive a historical root.
    fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError>;

    /// **[ext]** The retention window, oldest first.
    ///
    /// A host must choose between a latest-state and a historical-state
    /// provider per request; without this it discovers unavailability by
    /// failing a read.
    fn retention(&self) -> (CommitId, CommitId);

    /// **[ext]** Batched point read. reth's prewarm and parallel-execution
    /// paths issue many point reads per block; this is the cheapest
    /// throughput win available on the seam.
    fn get_many(
        &self,
        target: ReadTarget,
        keys: &[RecordKey],
        projection: Option<&[CellName]>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<Option<Record>>>, StoreError>;
}

/// One record's before/after across a commit, from [`StoreExt::changes`].
/// `None` on either side is creation or deletion respectively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordChange {
    pub key: RecordKey,
    pub before: Option<Record>,
    pub after: Option<Record>,
}

/// One write in a [`StoreExt::apply`] batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    Create {
        key: RecordKey,
        cells: Vec<(CellName, Cell)>,
    },
    Patch {
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
    },
    Delete {
        key: RecordKey,
        expected_version: Option<RecordVersion>,
    },
}

/// What one [`WriteOp`] produced. The batch is all-or-nothing, so a returned
/// vector is always the same length as the ops it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Created,
    Patched(RecordVersion),
    Deleted,
}

#[cfg(feature = "conformance")]
pub mod conformance;
