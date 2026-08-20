//! The Arkiv-native types every trait speaks in.
//!
//! Arkiv is more than its reth host, so its interfaces name Arkiv concepts —
//! [`UserAddress`], [`EntityAddress`], [`UserBalance`], [`UserNonce`],
//! [`EntityCreationNonce`] — never a host's primitives. A host maps them to its
//! own types at the boundary and keeps that mapping private: whatever address,
//! word, or big-integer types its storage engine uses, none of them ever cross
//! back out through these traits.
//!
//! The address types are plain arrays (no primitive library, `no_std`); the
//! numeric types are newtypes, so the compiler keeps unrelated quantities apart.

/// A **user's** 20-byte address — an entity's owner or creator, or a
/// transaction's caller: someone who holds a key and signs.
///
/// It just so happens to be equivalent to Ethereum's account addressing space
/// ([`EthAddress`]), which is what lets an Ethereum-style host use it verbatim —
/// but it is an Arkiv type, not a borrowed one. See
/// [`ETH_ADDRESS_LEN`](crate::constants::ETH_ADDRESS_LEN).
pub type UserAddress = [u8; 20];

/// A 20-byte **Ethereum** account address — deliberately Eth-prefixed, because
/// here Arkiv really does mean Ethereum's addressing and not its own: the value
/// an `addr` attribute
/// ([`EthereumAddress`](crate::entity::AttributeValue::EthereumAddress))
/// carries, with EIP-55 checksums and the ABI `address` word. The same width as
/// a [`UserAddress`] by coincidence of heritage, not by meaning.
pub type EthAddress = [u8; 20];

/// An **entity's** 32-byte address — its unique, stable identity, fixed at
/// creation.
///
/// Deliberately wider than a [`UserAddress`]: entity addresses are derived
/// (creator, nonce, and content mix into them), so they need a full hash-wide
/// space. A host whose native keying is narrower maps internally — the reth
/// host anchors each entity's MPT leaf by an address prefix — and that mapping
/// never crosses this API.
pub type EntityAddress = [u8; 32];

/// A 32-byte hash — a commitment root or a digest. Not an address: an
/// [`EntityAddress`] is the same width but names a thing, not a commitment.
pub type Hash = [u8; 32];

/// A block number. Expiry and lifecycle are counted in blocks, never wall-clock.
pub type BlockNumber = u64;

/// Gas / cost units. See [`CostModel`](crate::gas::CostModel).
pub type Gas = u64;

/// A user's 256-bit balance, as big-endian bytes.
///
/// Wide enough for any Ethereum-style balance, held as plain bytes so this crate
/// stays dependency-free; a host converts at the boundary (the reth host uses
/// `U256::from_be_bytes(balance.to_be_bytes())`, and back) and never exposes its
/// own numeric type. Big-endian makes the derived ordering the numeric one.
///
/// The arithmetic here is the little a business rule needs — *checked* for rules
/// that must reject (insufficient funds), *saturating* for accounting that must
/// not fail mid-block. Anything richer is host-side work for a real big-integer
/// type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct UserBalance([u8; 32]);

impl UserBalance {
    /// No funds.
    pub const ZERO: Self = Self([0; 32]);

    /// The largest representable balance — what saturating addition clamps to.
    pub const MAX: Self = Self([0xFF; 32]);

    pub const fn from_be_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn to_be_bytes(self) -> [u8; 32] {
        self.0
    }

    pub const fn from_u64(value: u64) -> Self {
        let mut bytes = [0u8; 32];
        let v = value.to_be_bytes();
        let mut i = 0;
        while i < 8 {
            bytes[24 + i] = v[i];
            i += 1;
        }
        Self(bytes)
    }

    pub const fn is_zero(self) -> bool {
        let mut i = 0;
        while i < 32 {
            if self.0[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// `self + rhs`, or `None` if the sum overflows 256 bits.
    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        let (sum, overflow) = self.overflowing_add(rhs);
        (!overflow).then_some(sum)
    }

    /// `self - rhs`, or `None` if `rhs > self`.
    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        let (diff, borrow) = self.borrowing_sub(rhs);
        (!borrow).then_some(diff)
    }

    /// `self + rhs`, clamped to [`UserBalance::MAX`].
    pub fn saturating_add(self, rhs: Self) -> Self {
        self.checked_add(rhs).unwrap_or(Self::MAX)
    }

    /// `self - rhs`, clamped to [`UserBalance::ZERO`].
    pub fn saturating_sub(self, rhs: Self) -> Self {
        self.checked_sub(rhs).unwrap_or(Self::ZERO)
    }

    /// Schoolbook byte-wise addition, most-significant byte last.
    fn overflowing_add(self, rhs: Self) -> (Self, bool) {
        let mut out = [0u8; 32];
        let mut carry = 0u16;
        for i in (0..32).rev() {
            let sum = self.0[i] as u16 + rhs.0[i] as u16 + carry;
            out[i] = sum as u8;
            carry = sum >> 8;
        }
        (Self(out), carry != 0)
    }

    /// Schoolbook byte-wise subtraction, most-significant byte last.
    fn borrowing_sub(self, rhs: Self) -> (Self, bool) {
        let mut out = [0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let diff = self.0[i] as i16 - rhs.0[i] as i16 - borrow;
            out[i] = (diff & 0xFF) as u8;
            borrow = i16::from(diff < 0);
        }
        (Self(out), borrow != 0)
    }
}

/// A user's **transaction** nonce — replay protection, advanced once per
/// transaction.
///
/// One of two nonces a user carries, and never interchangeable with the other
/// (the entity-minting [`EntityCreationNonce`]): they advance at different
/// rates — one batch bumps this once while creating three entities — so each
/// gets its own newtype and the compiler keeps them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct UserNonce(u64);

impl UserNonce {
    /// The nonce of a user who has never sent a transaction.
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// The nonce after one more transaction. Saturating: `u64::MAX`
    /// transactions from one account is unreachable, and wrapping would re-open
    /// old transactions to replay.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// A user's **entity-minting** nonce — how many entities that user has created,
/// and an input to every minted [`EntityAddress`].
///
/// The counterpart of [`UserNonce`] (see there for why the two are distinct
/// newtypes): mixing them up silently mints the wrong entity address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EntityCreationNonce(u64);

impl EntityCreationNonce {
    /// The nonce of a user who has never created an entity.
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// The nonce `count` creates later. Saturating: an account cannot realistically
    /// reach `u64::MAX` entities, and wrapping there would remint live keys.
    pub const fn advanced_by(self, count: u64) -> Self {
        Self(self.0.saturating_add(count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_add_and_sub_round_trip() {
        let a = UserBalance::from_u64(1_000_000);
        let b = UserBalance::from_u64(999);
        let sum = a.checked_add(b).unwrap();
        assert_eq!(sum, UserBalance::from_u64(1_000_999));
        assert_eq!(sum.checked_sub(b), Some(a));
    }

    /// The carry must propagate across byte (and u64-lane) boundaries, not just
    /// within the low bytes.
    #[test]
    fn balance_carry_crosses_byte_boundaries() {
        let max_u64 = UserBalance::from_u64(u64::MAX);
        let one = UserBalance::from_u64(1);
        let mut expected = [0u8; 32];
        expected[23] = 1; // 2^64
        assert_eq!(
            max_u64.checked_add(one),
            Some(UserBalance::from_be_bytes(expected))
        );
        assert_eq!(
            UserBalance::from_be_bytes(expected).checked_sub(one),
            Some(max_u64)
        );
    }

    #[test]
    fn balance_overflow_and_underflow_are_signalled() {
        assert_eq!(UserBalance::MAX.checked_add(UserBalance::from_u64(1)), None);
        assert_eq!(
            UserBalance::ZERO.checked_sub(UserBalance::from_u64(1)),
            None
        );
        assert_eq!(
            UserBalance::MAX.saturating_add(UserBalance::from_u64(1)),
            UserBalance::MAX
        );
        assert_eq!(
            UserBalance::ZERO.saturating_sub(UserBalance::from_u64(1)),
            UserBalance::ZERO
        );
    }

    /// Big-endian bytes make the derived `Ord` the numeric order.
    #[test]
    fn balance_orders_numerically() {
        assert!(UserBalance::from_u64(2) > UserBalance::from_u64(1));
        assert!(UserBalance::MAX > UserBalance::from_u64(u64::MAX));
        assert!(UserBalance::ZERO.is_zero());
        assert!(!UserBalance::from_u64(1).is_zero());
    }

    /// `next` never wraps — a wrapped transaction nonce would re-open replay.
    #[test]
    fn user_nonce_next_saturates() {
        assert_eq!(UserNonce::ZERO.next(), UserNonce::new(1));
        assert_eq!(UserNonce::new(u64::MAX).next(), UserNonce::new(u64::MAX));
    }
}
