use bytes::Bytes;
use core::fmt;
use malachitebft_proto::{Error as ProtoError, Protobuf};
use serde::{Deserialize, Serialize};

use crate::proto;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Serialize, Deserialize)]
pub struct ValueId([u8; 32]);

impl ValueId {
    pub const fn new(id: [u8; 32]) -> Self {
        Self(id)
    }
}

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl Protobuf for ValueId {
    type Proto = proto::ValueId;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        Ok(ValueId::new(proto.value.as_ref().try_into().map_err(
            |_| ProtoError::Other("value ID must be 32 bytes".into()),
        )?))
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::ValueId {
            value: self.0.to_vec().into(),
        })
    }
}

/// The value to decide on
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Value {
    pub value: [u8; 32],
    pub extensions: Bytes,
}

impl Value {
    /// Creates a new Value by hashing the complete payload with Keccak-256
    pub fn new(data: Bytes) -> Self {
        use sha3::{Digest, Keccak256};
        Self {
            value: Keccak256::digest(&data).into(),
            extensions: data,
        }
    }

    pub fn id(&self) -> ValueId {
        ValueId(self.value)
    }

    pub fn size_bytes(&self) -> usize {
        32 + self.extensions.len()
    }
}

impl malachitebft_core_types::Value for Value {
    type Id = ValueId;

    fn id(&self) -> ValueId {
        self.id()
    }
}

impl Protobuf for Value {
    type Proto = proto::Value;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        let value = Self::new(proto.extensions);
        if proto.value.as_ref() != value.value {
            return Err(ProtoError::Other("value hash mismatch".into()));
        }
        Ok(value)
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::Value {
            value: self.value.to_vec().into(),
            extensions: self.extensions.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_value_binds_full_payload_and_checks_digest() {
        let value = Value::new(Bytes::from_static(b"Arkiv payload"));
        let mut wire = value.to_proto().unwrap();
        assert_eq!(wire.value.len(), 32);
        assert_eq!(Value::from_proto(wire.clone()).unwrap(), value);
        wire.extensions = Bytes::from_static(b"different payload");
        assert!(Value::from_proto(wire).is_err());
        assert!(ValueId::from_proto(proto::ValueId {
            value: Bytes::from_static(b"short")
        })
        .is_err());
    }
}
