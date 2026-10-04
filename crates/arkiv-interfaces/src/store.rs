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
//! commit()     ── promote to a commit: head+1, durable
//! ```
//!
//! # Deviations from `golem-db-api.md`
//!
//! Everything marked **\[ext\]** is an addition this seam requires and the spec
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
    /// The **empty** state, before any commit. Note that this is not the chain's
    /// genesis *block*: see [`CHAIN_GENESIS`](Self::CHAIN_GENESIS).
    pub const GENESIS: Self = Self(0);

    /// The commit holding the chain's genesis allocation — prefunded accounts,
    /// and any seeded entities.
    ///
    /// Arkiv's genesis block is not empty, but commit 0 is, so the allocation
    /// lands as the first ordinary commit and `head() >= 1` *is* the
    /// "genesis applied" marker. No separate flag exists, and none is needed.
    ///
    /// The consequence to keep in mind: **commit ids run one ahead of block
    /// heights**, so block `n` is commit `n + 1`. That is the price of leaving
    /// commit 0 empty, and it is paid deliberately — see
    /// [`apply_genesis`].
    pub const CHAIN_GENESIS: Self = Self(1);
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
/// from [`Store::branch_digest`]**, so a write that leaves a record logically
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
    /// Pair a value with what producing it cost.
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
        let mut padded = [0u8; 32];
        let mut byte = 0;
        while byte < 20 {
            padded[12 + byte] = address[byte];
            byte += 1;
        }
        Self(padded)
    }

    /// The inverse of [`from_address`](Self::from_address). `None` if the
    /// leading 12 bytes are not zero, i.e. this key is not a padded address.
    pub const fn as_address(&self) -> Option<[u8; 20]> {
        let mut leading = 0;
        while leading < 12 {
            if self.0[leading] != 0 {
                return None;
            }
            leading += 1;
        }
        let mut address = [0u8; 20];
        let mut byte = 0;
        while byte < 20 {
            address[byte] = self.0[12 + byte];
            byte += 1;
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
    /// A single byte, `0` or `1`. Equality only.
    pub const BOOL: Self = Self(1);
    /// Four bytes, sign-bit-biased big-endian for ordering.
    pub const I32: Self = Self(2);
    /// Thirty-two bytes, plain big-endian.
    pub const U256: Self = Self(3);
    /// A 256-bit signed decimal, 18 places, sign-bit-biased big-endian.
    pub const DEC: Self = Self(4);
    /// Thirty-two opaque bytes. Equality only — no meaningful ordering.
    pub const BYTES32: Self = Self(5);
    /// Variable width, **field-only** — an attribute of this type is
    /// [`StoreError::InvalidArgument`].
    pub const BYTES: Self = Self(6);
    /// UTF-8 up to a cap. Equality and prefix; ordering is raw byte order.
    pub const STR: Self = Self(7);
    /// Eight bytes, plain big-endian. The block-number domain.
    pub const U64: Self = Self(8);

    /// The Arkiv profile's registered types.
    /// A 20-byte Ethereum address. Equality only.
    pub const ADDR: Self = Self(32);
    /// A 32-byte Arkiv entity key. Equality only.
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
    /// Whether `<`, `<=`, `>` and `>=` are answerable from this index.
    pub const fn supports_range(self) -> bool {
        matches!(self, Self::EqRange)
    }

    /// Whether a byte-prefix match is answerable from this index.
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
    /// An **indexed** cell: visible to `query` and `count`, and priced for
    /// the index maintenance it triggers.
    pub fn attribute(type_id: TypeId, value: Vec<u8>) -> Self {
        Self {
            kind: CellKind::Attribute,
            type_id,
            value,
        }
    }

    /// A **non-indexed** cell: invisible to queries, priced only for the
    /// bytes moved, and readable solely by point read.
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

/// What a [`seal`](Store::seal) computes: the commit this branch *would*
/// become, with nothing written yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealedCommit {
    /// The commit number this branch would take — `head + 1` at seal time.
    pub commit_nr: CommitId,
    /// The resulting state root. This is the block's state root.
    pub state_root: [u8; 32],
    /// The resulting index root.
    pub index_root: [u8; 32],
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
///
/// # This is a handle, not the data
///
/// Every method takes `&self`, writes included. A `Store` value is a *handle
/// to a database that lives elsewhere* — out of process, for the real one.
/// Writing through it changes the database, not the handle: `ValkeyStore`
/// holds no cell, lock or atomic, and a write leaves every one of its bytes
/// untouched.
///
/// So `&mut self` on a write was never describing mutation of the receiver.
/// It was claiming exclusive access to the database, which a handle does not
/// have and cannot get — the real store is a server, and this process is one
/// of its clients.
///
/// Concurrency control is therefore the store's own job, and it is already in
/// this API: [`StoreError::Conflict`]. `commit` fails if head moved, and the
/// lineage never forks — competing candidates are branches until one commits.
/// A `&mut self` seam adds a
/// compile-time exclusion on top of that, which is redundant where it holds
/// and false where it matters — it cannot reach another process.
///
/// What it costs is the thing a host actually needs: [`Arc<T>`] is a `Store`,
/// so one store can serve many concurrently open views.
///
/// Interior mutability is only needed by an implementation that *is* the
/// data — the in-memory reference store, which holds it in-process and
/// pays a `RefCell` for the pretence. That is a property of the reference
/// implementation, not of this seam.
///
/// [`Arc<T>`]: alloc::sync::Arc
pub trait Store: core::fmt::Debug {
    // -- commits and branches (unmetered) ----------------------------------

    /// The current canonical head.
    fn head(&self) -> CommitId;

    /// Open a root branch over canonical state. `at` defaults to head and must
    /// lie within the retention window. Root branches may [`commit`](Self::commit).
    ///
    /// Any number may be open concurrently over the same origin — which is what
    /// lets a host validate competing payloads at one height.
    fn begin(&self, at: Option<CommitId>) -> Result<BranchId, StoreError>;

    /// Seal the open frame and open the next. `O(1)`.
    ///
    /// Frames are the unit [`rollback`](Self::rollback) steps back over, which
    /// is how a host gets call-level atomicity: checkpoint before an operation,
    /// roll back if it fails, and nothing it wrote applies.
    fn checkpoint(&self, branch: BranchId) -> Result<(), StoreError>;

    /// Step back one checkpoint boundary, undoing the open frame.
    ///
    /// **Not idempotent**: calling it twice undoes two frames. Rolling back
    /// past the branch's first frame is [`StoreError::HandleInvalid`].
    fn rollback(&self, branch: BranchId) -> Result<(), StoreError>;

    /// Freeze the branch and compute its roots, **persisting nothing**.
    ///
    /// This is the half of a commit that a host needs *before* it knows whether
    /// the block will be adopted: it performs the merkleization a commit would,
    /// and stops short of writing. The branch stays readable and rejects
    /// writes afterwards.
    ///
    /// Any number of branches may be sealed over one head; whichever is adopted
    /// commits, and the rest leave no trace. That is what lets a host compute a
    /// block's state root while building it, compute the same root again while
    /// validating it, and only then decide.
    fn seal(&self, branch: BranchId) -> Result<SealedCommit, StoreError>;

    /// Drop a branch wholesale. Receipts already returned stay valid — cost is
    /// a return value, never state.
    fn discard(&self, branch: BranchId) -> Result<(), StoreError>;

    /// Promote a branch to the canonical head: implies a final
    /// [`checkpoint`](Self::checkpoint), and a [`seal`](Self::seal) if none was
    /// taken. The origin must still be head, else [`StoreError::Conflict`].
    /// Assigns `head + 1` atomically and consumes the handle.
    ///
    /// **There is no rewind.** A commit cannot be undone, so a host commits
    /// only blocks it will not reorg.
    fn commit(&self, root: BranchId) -> Result<CommitId, StoreError>;

    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo, StoreError>;

    // -- CRUD (metered) ----------------------------------------------------

    /// Insert a new record. Collision is [`StoreError::AlreadyExists`].
    ///
    /// Besides `cells`, the store writes the record's `#version` meta entry at
    /// `1` — its presence *is* record existence.
    fn create(
        &self,
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
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
        budget: Option<Budget>,
    ) -> Result<Metered<RecordVersion>, StoreError>;

    fn delete(
        &self,
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

    /// The branch's current content digest, computed on demand from the
    /// overlay. **Not incremental, and not `O(1)`.**
    ///
    /// A pure function of logical record content: `#`-prefixed meta entries,
    /// [`RecordVersion`] included, are **excluded**.
    ///
    /// This is a debugging and differential-testing tool, not the block's state
    /// root — that is [`SealedCommit::state_root`], which only a
    /// [`seal`](Self::seal) produces.
    fn branch_hash(&self, branch: BranchId) -> Result<[u8; 32], StoreError>;
}

/// Host-facing additions this seam needs and `golem-db-api.md` does not yet
/// provide. Each is **\[ext\]**; see `golem-db-api-reth-requirements.md`.
///
/// Kept separate from [`Store`] so the gap between the spec and what a reth
/// host requires stays legible — and shrinks visibly as the spec catches up.
pub trait StoreExt: Store {
    /// **\[ext R1\]** Commit a root branch, tagging the commit with an opaque
    /// 32-byte host identifier — for Arkiv, the block hash.
    ///
    /// reth addresses state by block hash everywhere (`state_by_block_hash`,
    /// forkchoice, the engine tree's keying), which a `u64` [`CommitId`]
    /// cannot express. The store attaches no meaning to the tag; it only has
    /// to keep it consistent with retention trimming, which is why this lives
    /// store-side rather than in a host map that could disagree after a crash.
    fn commit_tagged(&self, root: BranchId, tag: [u8; 32]) -> Result<CommitId, StoreError>;

    /// **\[ext R1\]** Resolve a tag back to its commit, if still retained.
    fn commit_by_tag(&self, tag: [u8; 32]) -> Result<Option<CommitId>, StoreError>;

    /// **\[ext R2\]** What one commit changed, as `(key, before, after)`.
    ///
    /// Three consumers: reth's `ChangeSetReader` / `StorageChangeSetReader`,
    /// which any provider the engine tree accepts must implement; txpool
    /// maintenance, which needs changed accounts on every block and reorg to
    /// evict stale-nonce and demote unaffordable transactions; and verifying
    /// a rewind did what it claimed.
    fn changes(&self, commit: CommitId) -> Result<Vec<RecordChange>, StoreError>;

    /// **\[ext R3\]** Apply many writes under one call and one receipt.
    ///
    /// A block is hundreds of individual calls and a genesis seed is
    /// thousands. Tolerable in-process; fatal across a process boundary, which
    /// is exactly where the A/B equivalence harness lives.
    fn apply(
        &self,
        branch: BranchId,
        ops: Vec<WriteOp>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<WriteOutcome>>, StoreError>;

    /// **\[ext R4\]** A committed digest, after the branch handle is gone.
    ///
    /// [`Store::branch_digest`] takes a branch, and `commit` consumes it — so
    /// there is otherwise no way to ask what commit `c` hashed to. Needed to
    /// validate a peer's block and to re-derive a historical root.
    fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError>;

    /// **\[ext\]** The retention window, oldest first.
    ///
    /// A host must choose between a latest-state and a historical-state
    /// provider per request; without this it discovers unavailability by
    /// failing a read.
    fn retention(&self) -> (CommitId, CommitId);

    /// **\[ext\]** Batched point read. reth's prewarm and parallel-execution
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

/// A shared handle is a store. This is the point of the `&self` seam: a host
/// opens one store and gives every view an `Arc` of it.
impl<T: Store + ?Sized> Store for alloc::sync::Arc<T> {
    fn head(&self) -> CommitId {
        (**self).head()
    }

    fn begin(&self, at: Option<CommitId>) -> Result<BranchId, StoreError> {
        (**self).begin(at)
    }

    fn checkpoint(&self, branch: BranchId) -> Result<(), StoreError> {
        (**self).checkpoint(branch)
    }

    fn rollback(&self, branch: BranchId) -> Result<(), StoreError> {
        (**self).rollback(branch)
    }

    fn seal(&self, branch: BranchId) -> Result<SealedCommit, StoreError> {
        (**self).seal(branch)
    }

    fn discard(&self, branch: BranchId) -> Result<(), StoreError> {
        (**self).discard(branch)
    }

    fn commit(&self, root: BranchId) -> Result<CommitId, StoreError> {
        (**self).commit(root)
    }

    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo, StoreError> {
        (**self).branch_info(branch)
    }

    fn create(
        &self,
        branch: BranchId,
        key: RecordKey,
        cells: Vec<(CellName, Cell)>,
        budget: Option<Budget>,
    ) -> Result<Metered<RecordKey>, StoreError> {
        (**self).create(branch, key, cells, budget)
    }

    fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        budget: Option<Budget>,
    ) -> Result<Metered<Option<Record>>, StoreError> {
        (**self).get(target, key, projection, budget)
    }

    fn patch(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        changes: Vec<(CellName, CellChange)>,
        budget: Option<Budget>,
    ) -> Result<Metered<RecordVersion>, StoreError> {
        (**self).patch(branch, key, expected_version, changes, budget)
    }

    fn delete(
        &self,
        branch: BranchId,
        key: RecordKey,
        expected_version: Option<RecordVersion>,
        budget: Option<Budget>,
    ) -> Result<Metered<()>, StoreError> {
        (**self).delete(branch, key, expected_version, budget)
    }

    fn query(
        &self,
        at: Option<CommitId>,
        query: &Query,
        budget: Option<Budget>,
    ) -> Result<Metered<QueryResult>, StoreError> {
        (**self).query(at, query, budget)
    }

    fn count(
        &self,
        at: Option<CommitId>,
        filter: &Filter,
        budget: Option<Budget>,
    ) -> Result<Metered<u64>, StoreError> {
        (**self).count(at, filter, budget)
    }

    fn branch_hash(&self, branch: BranchId) -> Result<[u8; 32], StoreError> {
        (**self).branch_hash(branch)
    }
}

impl<T: StoreExt + ?Sized> StoreExt for alloc::sync::Arc<T> {
    fn commit_tagged(&self, root: BranchId, tag: [u8; 32]) -> Result<CommitId, StoreError> {
        (**self).commit_tagged(root, tag)
    }

    fn commit_by_tag(&self, tag: [u8; 32]) -> Result<Option<CommitId>, StoreError> {
        (**self).commit_by_tag(tag)
    }

    fn changes(&self, commit: CommitId) -> Result<Vec<RecordChange>, StoreError> {
        (**self).changes(commit)
    }

    fn apply(
        &self,
        branch: BranchId,
        ops: Vec<WriteOp>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<WriteOutcome>>, StoreError> {
        (**self).apply(branch, ops, budget)
    }

    fn commit_hash(&self, commit: CommitId) -> Result<[u8; 32], StoreError> {
        (**self).commit_hash(commit)
    }

    fn retention(&self) -> (CommitId, CommitId) {
        (**self).retention()
    }

    fn get_many(
        &self,
        target: ReadTarget,
        keys: &[RecordKey],
        projection: Option<&[CellName]>,
        budget: Option<Budget>,
    ) -> Result<Metered<Vec<Option<Record>>>, StoreError> {
        (**self).get_many(target, keys, projection, budget)
    }
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

// ---------------------------------------------------------------------------
// Genesis
// ---------------------------------------------------------------------------

/// Write the chain's genesis allocation as [`CommitId::CHAIN_GENESIS`].
///
/// Genesis is not special to the store — it is the first ordinary commit — so
/// this is `begin` / `create` / `commit` and nothing more. It lives here rather
/// than on [`Store`] so every backend gets it for free and none can implement
/// it differently.
///
/// Refuses with [`StoreError::AlreadyExists`] unless the store is empty, which
/// is what makes it safe to call on every start-up: a node cannot half-apply
/// genesis, and cannot apply it twice over live state. `head() >= 1` afterwards
/// is the only "genesis applied" marker there is.
pub fn apply_genesis<S: Store>(
    store: &mut S,
    records: Vec<(RecordKey, Vec<(CellName, Cell)>)>,
) -> Result<CommitId, StoreError> {
    if store.head() != CommitId::GENESIS {
        return Err(StoreError::AlreadyExists);
    }

    let branch = store.begin(None)?;
    for (key, cells) in records {
        // Unbudgeted: genesis is not a metered transaction, it is the state
        // every metered transaction starts from.
        if let Err(error) = store.create(branch, key, cells, None) {
            // Leave nothing half-written for the next start-up to puzzle over.
            let _ = store.discard(branch);
            return Err(error);
        }
    }
    store.commit(branch)
}

// ---------------------------------------------------------------------------
// Matching — shared, because it is spec
// ---------------------------------------------------------------------------

/// The spec's `enc`: an encoding whose lexicographic byte order equals the
/// type's domain order.
///
/// Unsigned big-endian types already satisfy it. Signed ones do not — two's
/// complement puts negatives above positives bytewise — so their sign bit is
/// flipped, which is what makes range scans and sorting meaningful on a plain
/// sorted key-value store.
pub fn order_encoding(value: &[u8], type_id: TypeId) -> Vec<u8> {
    let mut encoded = value.to_vec();
    let signed = type_id == TypeId::I32 || type_id == TypeId::DEC;
    if signed && !encoded.is_empty() {
        encoded[0] ^= 0x80;
    }
    encoded
}

impl Predicate {
    /// Whether this predicate holds for `record`.
    ///
    /// A predicate only ever sees an **attribute of its own type**: fields are
    /// invisible to queries, and the type is part of the index bucket, so an
    /// attribute of some other type is a miss rather than a coercion.
    ///
    /// Returns [`StoreError::InvalidQuery`] if the operation is not one the
    /// type can be indexed for — checked before the record is consulted, so
    /// an invalid query fails the same way against any state.
    pub fn matches(&self, record: &Record) -> Result<bool, StoreError> {
        if !self.op.permitted_by(self.type_id.index_class()) {
            return Err(StoreError::InvalidQuery);
        }

        let hit = match record.cell(&self.cell) {
            Some(cell)
                if matches!(cell.kind, CellKind::Attribute) && cell.type_id == self.type_id =>
            {
                self.compare(&cell.value)
            }
            _ => false,
        };
        Ok(hit != self.negated)
    }

    /// Compare a stored value against this predicate's operand.
    ///
    /// Equality and prefix work on the canonical bytes directly; ordering
    /// works on [`order_encoding`], which for signed types is not the same.
    fn compare(&self, stored: &[u8]) -> bool {
        match self.op {
            CompareOp::Prefix => stored.starts_with(&self.value),
            CompareOp::Eq => stored == self.value.as_slice(),
            CompareOp::Lt | CompareOp::Lte | CompareOp::Gt | CompareOp::Gte => {
                let left = order_encoding(stored, self.type_id);
                let right = order_encoding(&self.value, self.type_id);
                match self.op {
                    CompareOp::Lt => left < right,
                    CompareOp::Lte => left <= right,
                    CompareOp::Gt => left > right,
                    CompareOp::Gte => left >= right,
                    CompareOp::Eq | CompareOp::Prefix => unreachable!("handled above"),
                }
            }
        }
    }
}

impl AndGroup {
    /// Whether every predicate in the group holds for `record`.
    ///
    /// Predicates are intersected in submitted order and the group stops at
    /// the first miss — the early exit the spec makes the caller's cost lever.
    pub fn matches(&self, record: &Record) -> Result<bool, StoreError> {
        for predicate in &self.0 {
            if !predicate.matches(record)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl Filter {
    /// Whether any group matches `record` — the OR half of the DNF.
    ///
    /// An empty filter matches every record.
    pub fn matches(&self, record: &Record) -> Result<bool, StoreError> {
        if self.0.is_empty() {
            return Ok(true);
        }
        for group in &self.0 {
            if group.matches(record)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Record {
    /// This record's value for a sort key, in [`order_encoding`] form.
    /// `None` when the record has no such cell, which sorts it last.
    pub fn sort_key(&self, cell: &str) -> Option<Vec<u8>> {
        self.cell(cell)
            .map(|cell| order_encoding(&cell.value, cell.type_id))
    }
}

#[cfg(feature = "conformance")]
pub mod conformance;

#[cfg(feature = "conformance")]
pub mod reference;
