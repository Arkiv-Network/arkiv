//! The byte-array types every trait speaks in.
//!
//! Plain arrays, no primitive library: a host maps them to its own types at the
//! boundary, e.g. `let addr: Address = alloy_addr.into_array();` (and back with
//! `Address::from`).

/// An **Ethereum account** address — an entity's owner or creator, or a
/// transaction's caller: accounts that hold a key and sign. A location Arkiv
/// *derives* is [`tyalias@Hash`]-wide instead; see
/// [`ETH_ADDRESS_LEN`](crate::constants::ETH_ADDRESS_LEN).
pub type Address = [u8; 20];

/// A 32-byte hash — a commitment root, an entity key, or a derived location.
pub type Hash = [u8; 32];

/// An entity's unique, stable identifier.
pub type EntityKey = Hash;

/// A block number. Expiry and lifecycle are counted in blocks, never wall-clock.
pub type BlockNumber = u64;

/// Gas / cost units. See [`CostModel`](crate::gas::CostModel).
pub type Gas = u64;

/// A 256-bit account balance, as big-endian bytes.
///
/// Wide enough for any Ethereum-style balance, held as plain bytes so this crate
/// stays dependency-free; a host converts at the boundary (e.g. alloy's
/// `U256::from_be_bytes(balance.to_be_bytes())`, and back). Big-endian makes the
/// derived ordering the numeric one.
///
/// The arithmetic here is the little a business rule needs — *checked* for rules
/// that must reject (insufficient funds), *saturating* for accounting that must
/// not fail mid-block. Anything richer is host-side work for a real big-integer
/// type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Balance([u8; 32]);

impl Balance {
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

    /// `self + rhs`, clamped to [`Balance::MAX`].
    pub fn saturating_add(self, rhs: Self) -> Self {
        self.checked_add(rhs).unwrap_or(Self::MAX)
    }

    /// `self - rhs`, clamped to [`Balance::ZERO`].
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

/// An owner's **entity-minting** nonce — how many entities that account has
/// created, and an input to every minted [`EntityKey`].
///
/// A distinct type because there are two unrelated nonces in play and mixing
/// them silently mints the wrong key: this one, and the account's ordinary
/// **transaction** nonce (reth's, incremented per transaction). They advance at
/// different rates — one batch can create three entities while bumping the tx
/// nonce once — so they are never interchangeable, and a bare `u64` on both
/// gives the compiler no way to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EntityNonce(u64);

impl EntityNonce {
    /// The nonce of an account that has never created an entity.
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
        let a = Balance::from_u64(1_000_000);
        let b = Balance::from_u64(999);
        let sum = a.checked_add(b).unwrap();
        assert_eq!(sum, Balance::from_u64(1_000_999));
        assert_eq!(sum.checked_sub(b), Some(a));
    }

    /// The carry must propagate across byte (and u64-lane) boundaries, not just
    /// within the low bytes.
    #[test]
    fn balance_carry_crosses_byte_boundaries() {
        let max_u64 = Balance::from_u64(u64::MAX);
        let one = Balance::from_u64(1);
        let mut expected = [0u8; 32];
        expected[23] = 1; // 2^64
        assert_eq!(
            max_u64.checked_add(one),
            Some(Balance::from_be_bytes(expected))
        );
        assert_eq!(
            Balance::from_be_bytes(expected).checked_sub(one),
            Some(max_u64)
        );
    }

    #[test]
    fn balance_overflow_and_underflow_are_signalled() {
        assert_eq!(Balance::MAX.checked_add(Balance::from_u64(1)), None);
        assert_eq!(Balance::ZERO.checked_sub(Balance::from_u64(1)), None);
        assert_eq!(
            Balance::MAX.saturating_add(Balance::from_u64(1)),
            Balance::MAX
        );
        assert_eq!(
            Balance::ZERO.saturating_sub(Balance::from_u64(1)),
            Balance::ZERO
        );
    }

    /// Big-endian bytes make the derived `Ord` the numeric order.
    #[test]
    fn balance_orders_numerically() {
        assert!(Balance::from_u64(2) > Balance::from_u64(1));
        assert!(Balance::MAX > Balance::from_u64(u64::MAX));
        assert!(Balance::ZERO.is_zero());
        assert!(!Balance::from_u64(1).is_zero());
    }
}
