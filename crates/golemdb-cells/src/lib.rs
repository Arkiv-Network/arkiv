//! Rust definitions for "Cells" in GolemDB, alongwith the logic for encoding and decoding them

/// A reference to an opaque cell with no guarantees about its content.
/// A lightweight wrapper around a byte slice, representing the raw data of a cell.
/// For validity and type guarantees, use `ParsedCellRef` instead.
pub struct OpaqueCellRef<'a> {
    pub data_ref: &'a [u8],
}

impl<'a> OpaqueCellRef<'a> {
    pub fn new(data_ref: &'a [u8]) -> Self {
        Self { data_ref }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.data_ref
    }

    pub fn len(&self) -> usize {
        self.data_ref.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data_ref.is_empty()
    }

    /// Convert this `OpaqueCellRef` into an owned `OpaqueCell` by copying the data.
    pub fn into_owned(self) -> OpaqueCell {
        OpaqueCell {
            data: self.data_ref.to_vec(),
        }
    }
}

/// An owned opaque cell, which is a vector of bytes.
pub struct OpaqueCell {
    pub data: Vec<u8>,
}

impl OpaqueCell {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if the cell is empty
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Convert this `OpaqueCell` into an `OpaqueCellRef` by borrowing the data.
    pub fn as_ref(&self) -> OpaqueCellRef<'_> {
        OpaqueCellRef {
            data_ref: &self.data,
        }
    }
}

pub enum CellParseError {
    InvalidFormat,
    InvalidType,
    UnknownType(String),
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeSize(u8);
const _: () = assert!(
    std::mem::size_of::<usize>() >= std::mem::size_of::<u8>(),
    "usize must be at least 8 bits"
);

impl TypeSize {
    pub fn new(size: usize) -> Self {
        if size == 0 || size > 256 {
            panic!("TypeSize must be between 1 and 256 bytes");
        }
        Self(size as u8 - 1) // Store as 0-255 internally
    }

    /// Create a `TypeSize` from a `u8`, where 0 represents 1 byte and 255 represents 256 bytes.
    pub fn from_u8(size: u8) -> Self {
        if size > 255 {
            panic!("TypeSize must be between 1 and 256 bytes");
        }
        Self(size) // Store as 0-255 internally
    }

    pub fn size(&self) -> usize {
        (self.0 + 1) as usize // Return as 1-256 externally
    }
}

#[non_exhaustive]
pub enum CellDataRef<'a> {
    Tombstone,
    Boolean(&'a [u8]),
    String(TypeSize, &'a [u8]),
    Bytes(TypeSize, &'a [u8]),
    UnsignedInteger(TypeSize, &'a [u8]),
    SignedInteger(TypeSize, &'a [u8]),
    Float(TypeSize, &'a [u8]),
    Date32(&'a [u8]),
    TimeStamp64(&'a [u8]),
    // --- for future expansion ---
    Custom(TypeSize, &'a [u8]), // For custom types
}

impl<'a> CellDataRef<'a> {
    pub fn try_from(
        type_data: (u8, Option<TypeSize>),
        raw_data: &'a [u8],
    ) -> Result<Self, CellParseError> {
        let type_discriminant =
            TypeDiscriminant::from_type_data_with_raw_data_verification(type_data, raw_data)?;
        match type_discriminant {
            TypeDiscriminant::Tombstone => Ok(Self::Tombstone),
            TypeDiscriminant::Boolean => Ok(Self::Boolean(&raw_data[..1])),
            TypeDiscriminant::String(ts) => Ok(Self::String(ts, &raw_data[..ts.size()])),
            TypeDiscriminant::Bytes(ts) => Ok(Self::Bytes(ts, &raw_data[..ts.size()])),
            TypeDiscriminant::UnsignedInteger(ts) => {
                Ok(Self::UnsignedInteger(ts, &raw_data[..ts.size()]))
            }
            TypeDiscriminant::SignedInteger(ts) => {
                Ok(Self::SignedInteger(ts, &raw_data[..ts.size()]))
            }
            TypeDiscriminant::Float(ts) => Ok(Self::Float(ts, &raw_data[..ts.size()])),
            TypeDiscriminant::Date32 => Ok(Self::Date32(&raw_data[..4])),
            TypeDiscriminant::TimeStamp64 => Ok(Self::TimeStamp64(&raw_data[..8])),
            TypeDiscriminant::Custom(ts) => Ok(Self::Custom(ts, &raw_data[..ts.size()])),
            _ => Err(CellParseError::UnknownType("".into())), // Handle unknown types
        }
    }

    pub fn len(&self) -> usize {
        match self {
            CellDataRef::Tombstone => 0,
            CellDataRef::Boolean(_) => 1,
            CellDataRef::String(size, _) => size.size(),
            CellDataRef::Bytes(size, _) => size.size(),
            CellDataRef::UnsignedInteger(size, _) => size.size(),
            CellDataRef::SignedInteger(size, _) => size.size(),
            CellDataRef::Float(size, _) => size.size(),
            CellDataRef::Date32(_) => 4,
            CellDataRef::TimeStamp64(_) => 8,
            CellDataRef::Custom(size, _) => size.size(),
            _ => 0, // For future expansion, default to 0
        }
    }
}

/// A discriminant for the type of data stored in a cell, used for parsing and validation.
#[non_exhaustive]
pub enum TypeDiscriminant {
    Tombstone,
    Boolean,
    /// Valid type sizes: 0-255, denoting 1-256 bytes.
    String(TypeSize),
    /// Valid type sizes: 0-255, denoting 1-256 bytes.
    Bytes(TypeSize),
    /// Valid type sizes: 8, 32, 64, 128, 256 bits (1, 4, 8, 16, 32 bytes).
    UnsignedInteger(TypeSize),
    /// Valid type sizes: 8, 32, 64, 128, 256 bits (1, 4, 8, 16, 32 bytes).
    SignedInteger(TypeSize),
    /// Valid type sizes: 32, 64, 128, 256 bits (4, 8, 16, 32 bytes).
    Decimal(TypeSize),
    /// Valid type sizes: 32, 64 bits (4, 8 bytes).
    Float(TypeSize),
    Date32,
    TimeStamp64,
    Custom(TypeSize),
}

impl TypeDiscriminant {
    pub fn from_type_data_with_raw_data_verification(
        type_data: (u8, Option<TypeSize>),
        raw_data: &[u8],
    ) -> Result<Self, CellParseError> {
        let (type_class, type_len) = type_data;
        match type_class {
            0x00 => Ok(TypeDiscriminant::Tombstone),
            0x01 => Ok(TypeDiscriminant::Boolean),
            0x02 => {
                let size = type_len.ok_or(CellParseError::InvalidFormat)?;
                Ok(TypeDiscriminant::String(size))
            }
            0x03 => {
                let size = type_len.ok_or(CellParseError::InvalidFormat)?;
                Ok(TypeDiscriminant::Bytes(size))
            }
            0x04 => {
                let ts = type_len.ok_or(CellParseError::InvalidFormat)?;
                if ts.size() != 8
                    || ts.size() != 32
                    || ts.size() != 64
                    || ts.size() != 128
                    || ts.size() != 256
                {
                    return Err(CellParseError::UnknownType(format!(
                        "UnsignedInteger:{}",
                        ts.size()
                    )));
                }
                Ok(TypeDiscriminant::UnsignedInteger(ts))
            }
            0x05 => {
                let ts = type_len.ok_or(CellParseError::InvalidFormat)?;
                if ts.size() != 8
                    || ts.size() != 32
                    || ts.size() != 64
                    || ts.size() != 128
                    || ts.size() != 256
                {
                    return Err(CellParseError::UnknownType(format!(
                        "SignedInteger:{}",
                        ts.size()
                    )));
                }
                Ok(TypeDiscriminant::SignedInteger(ts))
            }
            0x05 => {
                let ts = type_len.ok_or(CellParseError::InvalidFormat)?;
                if ts.size() != 32 || ts.size() != 64 || ts.size() != 128 || ts.size() != 256 {
                    return Err(CellParseError::UnknownType(format!(
                        "UnsignedInteger:{}",
                        ts.size()
                    )));
                }
                Ok(TypeDiscriminant::Decimal(ts))
            }
            0x06 => {
                let ts = type_len.ok_or(CellParseError::InvalidFormat)?;
                if ts.size() != 32 || ts.size() != 64 {
                    return Err(CellParseError::UnknownType(format!(
                        "UnsignedInteger:{}",
                        ts.size()
                    )));
                }
                Ok(TypeDiscriminant::Float(ts))
            }
            0x07 => Ok(TypeDiscriminant::Date32),
            0x08 => Ok(TypeDiscriminant::TimeStamp64),
            0x09 => {
                let size = type_len.ok_or(CellParseError::InvalidFormat)?;
                Ok(TypeDiscriminant::Custom(size))
            }
            _ => Err(CellParseError::InvalidType),
        }
    }
}

/// A cell which can be parsed into specific types.
pub struct ParsedCellRef<'a> {
    pub is_indexable: bool,
    pub data_ref: CellDataRef<'a>,
}

impl<'a> ParsedCellRef<'a> {
    /// Try to parse an `OpaqueCellRef` into a `ParsedCellRef`. A `ParsedCellRef`
    /// guarantees that the cell is valid and can be interpreted as a specific type.
    pub fn try_from(opaque: &'a OpaqueCellRef<'a>) -> Result<Self, CellParseError> {
        // A cell should have atleast:
        // - 1 byte for metadata + 1 byte of data = 2 bytes; or
        // - 2 bytes for metadata + 1 byte of data = 3 bytes
        //
        // The distinction between 1 byte and 2 byte metadata is the first bit of the first byte
        // [<is_multi_byte_metadata: 1 bit> <other_metadata: 7 bits>]
        let is_multi_byte_metadata =
            (opaque.len() > 0) && (opaque.as_bytes()[0] & 0b1000_0000 != 0);
        let minimum_bytes = 2 + if is_multi_byte_metadata { 1 } else { 0 };
        if opaque.len() < minimum_bytes {
            return Err(CellParseError::InvalidFormat);
        }

        // The second bit of the first byte indicates whether the cell is indexable or not.
        let is_indexable = opaque.as_bytes()[0] & 0b0100_0000 != 0;

        // First byte's certain bits are used to determine the type of the cell,
        // and the length of the type data if it's multi-byte metadata.
        let type_data = {
            let type_class = opaque.as_bytes()[0] & 0b0011_1111;
            let type_len = if is_multi_byte_metadata {
                Some(TypeSize::from_u8(opaque.as_bytes()[1]))
            } else {
                None
            };
            (type_class, type_len)
        };

        let raw_data = if is_multi_byte_metadata {
            &opaque.as_bytes()[2..]
        } else {
            &opaque.as_bytes()[1..]
        };

        let data_ref = CellDataRef::try_from(type_data, raw_data)?;

        Ok(Self {
            is_indexable,
            data_ref,
        })
    }
}
