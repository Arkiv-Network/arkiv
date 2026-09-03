//! Protocol-only wire types used between the block producer and followers.
//!
//! These calls are not part of the client-facing [`IEntityRegistry`] ABI.
//!
//! [`IEntityRegistry`]: crate::IEntityRegistry

alloy_sol_types::sol! {
    /// Removes expired entities selected by the block producer.
    function purgeExpired(bytes32[] entityKeys);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolCall;

    const PURGE_EXPIRED_SELECTOR: [u8; 4] = [0xad, 0x9e, 0x3f, 0x1c];

    #[test]
    fn purge_selector_is_pinned() {
        assert_eq!(purgeExpiredCall::SELECTOR, PURGE_EXPIRED_SELECTOR);
        assert_eq!(
            &alloy_primitives::keccak256("purgeExpired(bytes32[])")[..4],
            PURGE_EXPIRED_SELECTOR
        );
    }
}
