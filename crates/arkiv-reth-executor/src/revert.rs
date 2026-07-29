//! Revert-payload encoding — the executor's reasons → Solidity error data.
//!
//! The business logic reports failures as host-agnostic
//! [`RevertReason`]s and decode reports [`DecodeError`]s; this module maps both
//! onto the typed errors `IEntityRegistry` declares (`EntityNotFound`,
//! `NotOwner`, …) so SDK clients decode reverts the same way they would from
//! the retired contract. Reasons with no ABI counterpart (infrastructure
//! faults, out-of-gas) encode as the standard `Error(string)` so they stay
//! readable in any client.

use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::{Revert, SolError};
use arkiv_bindings::IEntityRegistry;
use arkiv_interfaces::execution::RevertReason;

use crate::decode::DecodeError;

/// Clip a `u64` block number into the ABI's `BlockNumber32` (`uint32`).
fn clip_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// ABI-encode a business-rule [`RevertReason`] as Solidity revert data.
pub fn revert_data(reason: &RevertReason) -> Bytes {
    use IEntityRegistry as E;
    match reason {
        RevertReason::NotFound { key } => E::EntityNotFound {
            entityKey: B256::from(*key),
        }
        .abi_encode(),
        RevertReason::NotOwner { key, caller, owner } => E::NotOwner {
            entityKey: B256::from(*key),
            caller: Address::from(*caller),
            owner: Address::from(*owner),
        }
        .abi_encode(),
        RevertReason::Expired { key, expires_at } => E::EntityExpired {
            entityKey: B256::from(*key),
            expiresAt: clip_u32(*expires_at),
        }
        .abi_encode(),
        RevertReason::NotExpired { key, expires_at } => E::EntityNotExpired {
            entityKey: B256::from(*key),
            expiresAt: clip_u32(*expires_at),
        }
        .abi_encode(),
        RevertReason::ExpiryNotExtended {
            key,
            new_expires_at,
            current_expires_at,
        } => E::ExpiryNotExtended {
            entityKey: B256::from(*key),
            newExpiresAt: clip_u32(*new_expires_at),
            currentExpiresAt: clip_u32(*current_expires_at),
        }
        .abi_encode(),
        RevertReason::TransferToSelf { key } => E::TransferToSelf {
            entityKey: B256::from(*key),
        }
        .abi_encode(),
        // No ABI counterpart — the standard `Error(string)` keeps them readable.
        RevertReason::AlreadyExists { .. } | RevertReason::OutOfGas => {
            Revert::from(reason.to_string()).abi_encode()
        }
    }
    .into()
}

/// ABI-encode a structural [`DecodeError`] as Solidity revert data.
pub fn decode_revert_data(e: &DecodeError) -> Bytes {
    use IEntityRegistry as E;
    match e {
        DecodeError::EmptyBatch => E::EmptyBatch {}.abi_encode(),
        DecodeError::ZeroBtl => E::ZeroBtl {}.abi_encode(),
        DecodeError::InvalidOpType(t) => E::InvalidOpType { operationType: *t }.abi_encode(),
        DecodeError::TransferToZeroAddress { key } => E::TransferToZeroAddress {
            entityKey: B256::from(*key),
        }
        .abi_encode(),
        DecodeError::InvalidAttributeValue {
            name, value_type, ..
        } => E::InvalidValueType {
            name: (*name).into(),
            valueType: *value_type,
        }
        .abi_encode(),
        DecodeError::TooManyAttributes { count, max } => E::TooManyAttributes {
            count: alloy_primitives::U256::from(*count),
            maxCount: alloy_primitives::U256::from(*max),
        }
        .abi_encode(),
        DecodeError::AttributesNotSorted => E::AttributesNotSorted {}.abi_encode(),
        DecodeError::AttributeNameEmpty => E::Ident32Empty {}.abi_encode(),
        DecodeError::AttributeNameInvalidByte { position, value } => E::Ident32InvalidByte {
            position: alloy_primitives::U256::from(*position),
            value: alloy_primitives::FixedBytes::<1>::from([*value]),
        }
        .abi_encode(),
        // Structural faults with no ABI counterpart.
        DecodeError::CalldataTooShort | DecodeError::UnknownSelector(_) | DecodeError::Abi(_) => {
            Revert::from(e.to_string()).abi_encode()
        }
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector(data: &Bytes) -> [u8; 4] {
        data[..4].try_into().unwrap()
    }

    #[test]
    fn business_reasons_encode_as_typed_errors() {
        use IEntityRegistry as E;
        let key = [0x11u8; 32];

        let data = revert_data(&RevertReason::NotFound { key });
        assert_eq!(selector(&data), E::EntityNotFound::SELECTOR);
        let decoded = E::EntityNotFound::abi_decode(&data).unwrap();
        assert_eq!(decoded.entityKey, B256::from(key));

        let data = revert_data(&RevertReason::NotOwner {
            key,
            caller: [0xAA; 20],
            owner: [0xBB; 20],
        });
        let decoded = E::NotOwner::abi_decode(&data).unwrap();
        assert_eq!(decoded.caller, Address::repeat_byte(0xAA));
        assert_eq!(decoded.owner, Address::repeat_byte(0xBB));

        let data = revert_data(&RevertReason::Expired {
            key,
            expires_at: 42,
        });
        assert_eq!(E::EntityExpired::abi_decode(&data).unwrap().expiresAt, 42);

        let data = revert_data(&RevertReason::ExpiryNotExtended {
            key,
            new_expires_at: 5,
            current_expires_at: 9,
        });
        let decoded = E::ExpiryNotExtended::abi_decode(&data).unwrap();
        assert_eq!(decoded.newExpiresAt, 5);
        assert_eq!(decoded.currentExpiresAt, 9);

        let data = revert_data(&RevertReason::TransferToSelf { key });
        assert_eq!(selector(&data), E::TransferToSelf::SELECTOR);

        let data = revert_data(&RevertReason::NotExpired { key, expires_at: 7 });
        assert_eq!(selector(&data), E::EntityNotExpired::SELECTOR);
    }

    #[test]
    fn reasons_without_abi_counterpart_encode_as_error_string() {
        let data = revert_data(&RevertReason::OutOfGas);
        let decoded = Revert::abi_decode(&data).unwrap();
        assert_eq!(decoded.reason, "out of gas");
    }

    #[test]
    fn decode_errors_encode_as_typed_errors() {
        use IEntityRegistry as E;

        assert_eq!(
            selector(&decode_revert_data(&DecodeError::EmptyBatch)),
            E::EmptyBatch::SELECTOR
        );
        assert_eq!(
            selector(&decode_revert_data(&DecodeError::ZeroBtl)),
            E::ZeroBtl::SELECTOR
        );
        assert_eq!(
            selector(&decode_revert_data(&DecodeError::AttributesNotSorted)),
            E::AttributesNotSorted::SELECTOR
        );

        let data = decode_revert_data(&DecodeError::TooManyAttributes { count: 40, max: 32 });
        let decoded = E::TooManyAttributes::abi_decode(&data).unwrap();
        assert_eq!(decoded.count, alloy_primitives::U256::from(40));
        assert_eq!(decoded.maxCount, alloy_primitives::U256::from(32));

        let data = decode_revert_data(&DecodeError::AttributeNameInvalidByte {
            position: 4,
            value: 0x49,
        });
        let decoded = E::Ident32InvalidByte::abi_decode(&data).unwrap();
        assert_eq!(decoded.position, alloy_primitives::U256::from(4));
        assert_eq!(
            decoded.value,
            alloy_primitives::FixedBytes::<1>::from([0x49])
        );

        assert_eq!(
            selector(&decode_revert_data(&DecodeError::AttributeNameEmpty)),
            E::Ident32Empty::SELECTOR
        );

        let data = decode_revert_data(&DecodeError::UnknownSelector([0xDE, 0xAD, 0xBE, 0xEF]));
        assert!(
            Revert::abi_decode(&data).is_ok(),
            "infra fault → Error(string)"
        );
    }

    #[test]
    fn u64_expiries_clip_to_u32() {
        assert_eq!(clip_u32(u64::MAX), u32::MAX);
        assert_eq!(clip_u32(7), 7);
    }
}
