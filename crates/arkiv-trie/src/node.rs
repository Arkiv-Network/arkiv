//! The Merkle-Patricia node codec: paths as nibbles of any length, and the
//! standard RLP node shapes. `alloy-trie`'s types cap keys at 32 bytes, and
//! Arkiv's index keys are longer, so this is written out here. The byte
//! format is Ethereum's, so every root is a standard one.

use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Encodable, Header};
use core::cmp::Ordering;

/// A key path as nibbles, one per byte, each `< 16`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Path(Vec<u8>);

impl core::fmt::Debug for Path {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for n in &self.0 {
            write!(f, "{n:x}")?;
        }
        Ok(())
    }
}

impl Path {
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// The nibbles of `bytes`, high nibble first.
    pub fn unpack(bytes: &[u8]) -> Self {
        let mut out = Vec::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(b >> 4);
            out.push(b & 0x0f);
        }
        Self(out)
    }

    pub fn from_nibbles(nibbles: Vec<u8>) -> Self {
        debug_assert!(nibbles.iter().all(|n| *n < 16));
        Self(nibbles)
    }

    /// Back to bytes. The path must hold an even number of nibbles.
    pub fn pack(&self) -> Vec<u8> {
        debug_assert!(self.0.len().is_multiple_of(2));
        self.0
            .chunks(2)
            .map(|c| (c[0] << 4) | c.get(1).copied().unwrap_or(0))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<u8> {
        self.0.get(i).copied()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn slice(&self, range: core::ops::Range<usize>) -> Self {
        Self(self.0[range].to_vec())
    }

    pub fn slice_from(&self, start: usize) -> Self {
        Self(self.0[start..].to_vec())
    }

    pub fn push(&mut self, nibble: u8) {
        debug_assert!(nibble < 16);
        self.0.push(nibble);
    }

    pub fn extend(&mut self, other: &Self) {
        self.0.extend_from_slice(&other.0);
    }

    pub fn starts_with(&self, prefix: &Self) -> bool {
        self.0.starts_with(&prefix.0)
    }

    pub fn common_prefix_length(&self, other: &Self) -> usize {
        self.0
            .iter()
            .zip(&other.0)
            .take_while(|(a, b)| a == b)
            .count()
    }

    /// Compare the first `n` nibbles of each.
    pub fn cmp_prefix(&self, other: &Self, n: usize) -> Ordering {
        let a = &self.0[..n.min(self.0.len())];
        let b = &other.0[..n.min(other.0.len())];
        a.cmp(b)
    }

    /// The hex-prefix encoding of the path, with the leaf flag.
    fn hex_prefix(&self, leaf: bool) -> Vec<u8> {
        let odd = self.0.len() % 2 == 1;
        let mut out = Vec::with_capacity(self.0.len() / 2 + 1);
        let flag = (if leaf { 2 } else { 0 }) | u8::from(odd);
        let (first, rest) = if odd {
            ((flag << 4) | self.0[0], &self.0[1..])
        } else {
            (flag << 4, &self.0[..])
        };
        out.push(first);
        for c in rest.chunks(2) {
            out.push((c[0] << 4) | c[1]);
        }
        out
    }

    /// Decode a hex-prefix path; returns the path and whether it is a leaf.
    fn from_hex_prefix(bytes: &[u8]) -> Result<(Self, bool), DecodeError> {
        let Some(&first) = bytes.first() else {
            return Err(DecodeError::BadPath);
        };
        let flag = first >> 4;
        if flag > 3 {
            return Err(DecodeError::BadPath);
        }
        let leaf = flag & 2 != 0;
        let odd = flag & 1 != 0;
        let mut nibbles = Vec::with_capacity(bytes.len() * 2);
        if odd {
            nibbles.push(first & 0x0f);
        }
        for b in &bytes[1..] {
            nibbles.push(b >> 4);
            nibbles.push(b & 0x0f);
        }
        Ok((Self(nibbles), leaf))
    }
}

/// How a parent refers to a child: by hash when the child's encoding is 32
/// bytes or more, inline otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Child {
    Hash(B256),
    /// The child's own RLP, embedded in the parent.
    Inline(Vec<u8>),
}

impl Child {
    /// The reference for a node whose encoding is `rlp`.
    pub fn of(rlp: &[u8]) -> Self {
        if rlp.len() < 32 {
            Self::Inline(rlp.to_vec())
        } else {
            Self::Hash(keccak256(rlp))
        }
    }

    pub fn as_hash(&self) -> Option<B256> {
        match self {
            Self::Hash(h) => Some(*h),
            Self::Inline(_) => None,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Hash(h) => h.as_slice().encode(out),
            Self::Inline(rlp) => out.extend_from_slice(rlp),
        }
    }
}

/// A trie node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Leaf { path: Path, value: Vec<u8> },
    Extension { path: Path, child: Child },
    Branch { children: Box<[Option<Child>; 16]> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Rlp(alloy_rlp::Error),
    BadPath,
    BadShape,
    BadChild,
}

impl From<alloy_rlp::Error> for DecodeError {
    fn from(e: alloy_rlp::Error) -> Self {
        Self::Rlp(e)
    }
}

impl Node {
    pub fn branch(children: [Option<Child>; 16]) -> Self {
        Self::Branch {
            children: Box::new(children),
        }
    }

    /// The standard RLP encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        match self {
            Self::Leaf { path, value } => {
                path.hex_prefix(true).as_slice().encode(&mut payload);
                value.as_slice().encode(&mut payload);
            }
            Self::Extension { path, child } => {
                path.hex_prefix(false).as_slice().encode(&mut payload);
                child.encode(&mut payload);
            }
            Self::Branch { children } => {
                for child in children.iter() {
                    match child {
                        Some(c) => c.encode(&mut payload),
                        None => payload.push(alloy_rlp::EMPTY_STRING_CODE),
                    }
                }
                payload.push(alloy_rlp::EMPTY_STRING_CODE);
            }
        }
        let mut out = Vec::with_capacity(payload.len() + 4);
        Header {
            list: true,
            payload_length: payload.len(),
        }
        .encode(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    pub fn decode(rlp: &[u8]) -> Result<Self, DecodeError> {
        let mut buf = rlp;
        let header = Header::decode(&mut buf)?;
        if !header.list || buf.len() < header.payload_length {
            return Err(DecodeError::BadShape);
        }
        let mut items = &buf[..header.payload_length];
        let mut fields: Vec<Item<'_>> = Vec::with_capacity(17);
        while !items.is_empty() {
            fields.push(Item::take(&mut items)?);
        }
        match fields.len() {
            2 => {
                let Item::Str(hp) = fields[0] else {
                    return Err(DecodeError::BadShape);
                };
                let (path, leaf) = Path::from_hex_prefix(hp)?;
                if leaf {
                    let Item::Str(value) = fields[1] else {
                        return Err(DecodeError::BadShape);
                    };
                    Ok(Self::Leaf {
                        path,
                        value: value.to_vec(),
                    })
                } else {
                    let child = fields[1].child()?.ok_or(DecodeError::BadChild)?;
                    Ok(Self::Extension { path, child })
                }
            }
            17 => {
                let mut children: [Option<Child>; 16] = Default::default();
                for (slot, item) in children.iter_mut().zip(&fields[..16]) {
                    *slot = item.child()?;
                }
                Ok(Self::branch(children))
            }
            _ => Err(DecodeError::BadShape),
        }
    }
}

/// One RLP item of a node's list: a string, or an embedded list (an inline
/// child), kept as its raw bytes.
enum Item<'a> {
    Str(&'a [u8]),
    List(&'a [u8]),
}

impl<'a> Item<'a> {
    fn take(buf: &mut &'a [u8]) -> Result<Self, DecodeError> {
        let start = *buf;
        let header = Header::decode(buf)?;
        if buf.len() < header.payload_length {
            return Err(DecodeError::BadShape);
        }
        let payload = &buf[..header.payload_length];
        *buf = &buf[header.payload_length..];
        if header.list {
            let total = start.len() - buf.len();
            Ok(Self::List(&start[..total]))
        } else {
            Ok(Self::Str(payload))
        }
    }

    fn child(&self) -> Result<Option<Child>, DecodeError> {
        match self {
            Self::Str([]) => Ok(None),
            Self::Str(s) if s.len() == 32 => Ok(Some(Child::Hash(B256::from_slice(s)))),
            Self::Str(_) => Err(DecodeError::BadChild),
            Self::List(raw) => Ok(Some(Child::Inline(raw.to_vec()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_prefix_round_trips() {
        for (nibbles, leaf) in [
            (vec![], false),
            (vec![1], true),
            (vec![1, 2], false),
            (vec![1, 2, 3], true),
            (vec![0xf, 0, 0xa, 0xb, 0xc], false),
        ] {
            let p = Path::from_nibbles(nibbles.clone());
            let hp = p.hex_prefix(leaf);
            let (back, l) = Path::from_hex_prefix(&hp).unwrap();
            assert_eq!(back.0, nibbles);
            assert_eq!(l, leaf);
        }
    }

    #[test]
    fn nodes_round_trip_through_rlp() {
        let leaf = Node::Leaf {
            path: Path::unpack(&[1, 2, 3]),
            value: vec![9; 40],
        };
        let rlp = leaf.encode();
        assert_eq!(Node::decode(&rlp).unwrap(), leaf);

        let small = Node::Leaf {
            path: Path::from_nibbles(vec![7]),
            value: vec![1],
        };
        let ext = Node::Extension {
            path: Path::from_nibbles(vec![0xa, 0xb, 0xc]),
            child: Child::of(&leaf.encode()),
        };
        let rlp = ext.encode();
        assert_eq!(Node::decode(&rlp).unwrap(), ext);

        let mut children: [Option<Child>; 16] = Default::default();
        children[3] = Some(Child::of(&small.encode()));
        children[0xf] = Some(Child::of(&leaf.encode()));
        let branch = Node::branch(children);
        let rlp = branch.encode();
        assert_eq!(Node::decode(&rlp).unwrap(), branch);
        assert!(matches!(
            &Node::decode(&rlp).unwrap(),
            Node::Branch { children } if matches!(children[3], Some(Child::Inline(_)))
        ));
    }
}
