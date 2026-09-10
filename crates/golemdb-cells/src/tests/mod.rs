//! The unit tests: fixed vectors and hand-written cases.
//!
//! - [`vectors`] holds the tables, accept and reject.
//! - [`properties`] holds what must hold over *generated* input instead.

mod properties;
mod vectors;

use crate::*;
use vectors::*;

#[test]
fn vectors_decode_to_their_stated_meaning() {
    for (name, bytes, indexable, ty, value) in accept_vectors() {
        let cell = CellValue::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(cell.cell_type(), *ty, "{name}: type");
        assert_eq!(cell.value(), *value, "{name}: value");
        assert_eq!(cell.is_indexable(), *indexable, "{name}: indexable");
        assert_eq!(cell.metadata(), bytes[0], "{name}: metadata byte");
    }
}

#[test]
fn vectors_re_encode_to_the_same_bytes() {
    for (name, bytes, indexable, ty, value) in accept_vectors() {
        let cell = CellValue::new(*ty, value, *indexable).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(cell.encode(), *bytes, "{name}");
    }
}

/// The type ids each cover a distinct id, and the set is the spec's.
#[test]
fn vectors_cover_every_core_family() {
    let mut seen: Vec<u8> = VECTORS
        .iter()
        .map(|(_, b, ..)| b[0] & TYPE_ID_MASK)
        .collect();
    seen.sort_unstable();
    seen.dedup();
    let expected: Vec<u8> = (0..TYPE_ID_SPACE)
        .filter(|id| CellType::from_id(*id).is_ok() && *id < CUSTOM_TYPE_ID_BASE)
        .collect();
    assert!(
        expected.iter().all(|id| seen.contains(id)),
        "uncovered core ids: {:?}",
        expected
            .iter()
            .filter(|id| !seen.contains(id))
            .collect::<Vec<_>>()
    );
}

/// Names are the spec table's, id for id — what `LengthMismatch` and other
/// diagnostics print.
#[test]
fn type_names_match_the_spec() {
    #[rustfmt::skip]
    const NAMES: &[(u8, &str)] = &[
        (0, "tombstone"), (1, "bool"), (2, "str"), (3, "bytes"), (4, "bytes20"),
        (8, "bytes4"), (9, "bytes8"), (10, "bytes16"), (11, "bytes32"),
        (12, "u32"), (13, "u64"), (14, "u128"), (15, "u256"),
        (16, "i32"), (17, "i64"), (18, "i128"), (19, "i256"),
        (20, "dec32"), (21, "dec64"), (22, "dec128"), (23, "dec256"),
        (24, "f32"), (25, "f64"), (28, "date32"), (29, "timestamp64"),
    ];

    for (id, name) in NAMES {
        assert_eq!(CellType::from_id(*id).unwrap().name(), *name, "id {id}");
    }

    // Every core id is named, and no two share a name.
    let named: Vec<u8> = NAMES.iter().map(|(id, _)| *id).collect();
    for id in 0..CUSTOM_TYPE_ID_BASE {
        assert_eq!(
            CellType::from_id(id).is_ok(),
            named.contains(&id),
            "id {id}"
        );
    }
    let mut names: Vec<&str> = NAMES.iter().map(|(_, n)| *n).collect();
    names.sort_unstable();
    let count = names.len();
    names.dedup();
    assert_eq!(names.len(), count, "two types share a name");
}

/// Every id round-trips through `from_id`/`id`, and the reserved slots are
/// exactly the ones the spec lists.
#[test]
fn id_space_matches_the_spec() {
    const RESERVED: &[u8] = &[5, 6, 7, 26, 27, 30, 31];
    // With `custom_types` off the top block decodes to nothing either, but
    // as `CustomTypesDisabled` rather than `ReservedType`.
    let custom_off = cfg!(not(feature = "custom_types"));
    for id in 0..TYPE_ID_SPACE {
        let reserved = RESERVED.contains(&id) || (32..64).contains(&id);
        let disabled = custom_off && id >= CUSTOM_TYPE_ID_BASE;
        match CellType::from_id(id) {
            Ok(ty) => {
                assert!(!reserved && !disabled, "id {id} should not decode");
                assert_eq!(ty.id(), id);
            }
            Err(e) if disabled => {
                assert_eq!(e, CellParseError::CustomTypesDisabled(id));
            }
            Err(e) => {
                assert!(reserved, "id {id} should decode");
                assert_eq!(e, CellParseError::ReservedType(id));
            }
        }
    }
}

/// Exhaustive over the whole metadata byte and every value length up to
/// past the widest fixed type: parsing never panics, a cell that parses
/// re-encodes to the exact bytes it came from, and acceptance agrees with
/// the layout table.
#[test]
fn every_metadata_byte_and_length() {
    // 0x01 is a valid `bool`, valid UTF-8, and a valid byte anywhere else,
    // so length is the only thing under test.
    let payload = [0x01u8; 40];
    for metadata in 0..=u8::MAX {
        for len in 0..=payload.len() {
            let mut bytes = vec![metadata];
            bytes.extend_from_slice(&payload[..len]);

            let accepted = match CellType::from_id(metadata & TYPE_ID_MASK) {
                Err(_) => false,
                Ok(ty) => match ty.layout() {
                    ValueLayout::Fixed(n) => len == n,
                    // The first payload byte is the length byte, and it
                    // must account for every byte after it.
                    ValueLayout::LengthPrefixed { .. } => {
                        len >= 1 && payload[0] as usize == len - 1
                    }
                },
            };

            match CellValue::parse(&bytes) {
                Ok(cell) => {
                    assert!(accepted, "0x{metadata:02X} len {len}: should have failed");
                    assert_eq!(cell.encode(), bytes, "0x{metadata:02X} len {len}");
                }
                Err(_) => assert!(!accepted, "0x{metadata:02X} len {len}: should parse"),
            }
        }
    }
}

#[test]
fn bad_vectors_are_rejected_for_the_stated_reason() {
    for (name, bytes, expected) in BAD_VECTORS {
        let got = CellValue::parse(bytes)
            .map(|c| c.cell_type())
            .expect_err(&format!("{name}: parsed, should have failed"));
        assert_eq!(got, *expected, "{name}");
    }
}

/// Each accessor decodes its own type's value.
#[test]
fn accessors_decode_their_rust_equivalents() {
    fn parse(bytes: &[u8]) -> CellValue<'_> {
        CellValue::parse(bytes).unwrap()
    }

    assert_eq!(parse(&[0x01, 1]).as_bool(), Some(true));
    assert_eq!(parse(&[0x01, 0]).as_bool(), Some(false));
    assert_eq!(parse(&[0x02, 2, b'h', b'i']).as_str(), Some("hi"));
    assert_eq!(
        parse(&[0x03, 2, 0xDE, 0xAD]).as_bytes(),
        Some(&[0xDE, 0xAD][..])
    );

    assert_eq!(parse(&[0x08, 1, 2, 3, 4]).as_bytes4(), Some([1, 2, 3, 4]));
    assert_eq!(
        parse(&[&[0x04][..], &[7u8; 20]].concat()).as_bytes20(),
        Some([7u8; 20])
    );
    assert_eq!(
        parse(&[&[0x0B][..], &[9u8; 32]].concat()).as_bytes32(),
        Some([9u8; 32])
    );

    assert_eq!(
        parse(&[&[0x0C][..], &7u32.to_be_bytes()].concat()).as_u32(),
        Some(7)
    );
    assert_eq!(
        parse(&[&[0x0D][..], &u64::MAX.to_be_bytes()].concat()).as_u64(),
        Some(u64::MAX)
    );
    assert_eq!(
        parse(&[&[0x0E][..], &1u128.to_be_bytes()].concat()).as_u128(),
        Some(1)
    );
    assert_eq!(
        parse(&[&[0x0F][..], &[0xFFu8; 32]].concat()).as_u256_be(),
        Some([0xFFu8; 32])
    );

    // Signed values are plain two's complement, so negatives round-trip.
    assert_eq!(
        parse(&[&[0x10][..], &(-1i32).to_be_bytes()].concat()).as_i32(),
        Some(-1)
    );
    assert_eq!(
        parse(&[&[0x11][..], &i64::MIN.to_be_bytes()].concat()).as_i64(),
        Some(i64::MIN)
    );
    assert_eq!(
        parse(&[&[0x12][..], &(-42i128).to_be_bytes()].concat()).as_i128(),
        Some(-42)
    );

    assert_eq!(
        parse(&[&[0x14][..], &(-5i32).to_be_bytes()].concat()).as_dec32_unscaled(),
        Some(-5)
    );

    assert_eq!(
        parse(&[&[0x18][..], &1.5f32.to_be_bytes()].concat()).as_f32(),
        Some(1.5)
    );
    assert_eq!(
        parse(&[&[0x19][..], &(-0.25f64).to_be_bytes()].concat()).as_f64(),
        Some(-0.25)
    );

    assert_eq!(
        parse(&[&[0x1C][..], &20_000i32.to_be_bytes()].concat()).as_date32(),
        Some(20_000)
    );
    assert_eq!(
        parse(&[&[0x1D][..], &(-1i64).to_be_bytes()].concat()).as_timestamp64(),
        Some(-1)
    );
}

/// An accessor answers only for its own type. Same eight bytes under `u64`,
/// `i64`, `dec64`, `f64`, `bytes8` and `timestamp64` — each reads out under
/// exactly one of them, so no cell is ever silently reinterpreted.
#[test]
fn accessors_are_exclusive() {
    let payload = [0x80u8, 0, 0, 0, 0, 0, 0, 1];
    for id in [0x09u8, 0x0D, 0x11, 0x15, 0x19, 0x1D] {
        let bytes = [&[id][..], &payload].concat();
        let cell = CellValue::parse(&bytes).unwrap();

        let hits = [
            cell.as_bytes8().is_some(),
            cell.as_u64().is_some(),
            cell.as_i64().is_some(),
            cell.as_dec64_unscaled().is_some(),
            cell.as_f64().is_some(),
            cell.as_timestamp64().is_some(),
        ]
        .iter()
        .filter(|hit| **hit)
        .count();
        assert_eq!(hits, 1, "id {id:#04x} answered {hits} accessors");

        // Nor do the wrong-width or wrong-family accessors answer.
        assert_eq!(cell.as_u32(), None, "id {id:#04x}");
        assert_eq!(cell.as_u128(), None, "id {id:#04x}");
        assert_eq!(cell.as_bool(), None, "id {id:#04x}");
        assert_eq!(cell.as_str(), None, "id {id:#04x}");
        assert_eq!(cell.as_bytes(), None, "id {id:#04x}");
    }
}

/// The message quotes the offending bytes as hex, positioned, and says so
/// when the value runs past the window.
#[test]
fn invalid_utf8_message_shows_the_bytes() {
    let msg = |cell: &[u8]| CellValue::parse(cell).unwrap_err().to_string();

    assert_eq!(
        msg(&[0x02, 3, 0xED, 0xA0, 0x80]),
        "str is not valid UTF-8 at byte 0 of 3: ed a0 80"
    );
    // The window starts at the failure, not at the value's start.
    assert_eq!(
        msg(&[0x02, 4, b'h', b'i', 0xC0, 0xAF]),
        "str is not valid UTF-8 at byte 2 of 4: c0 af"
    );

    // A value longer than the window is cut, and marked as cut.
    let mut long = vec![0x02, 40];
    long.extend_from_slice(&[b'a'; 20]);
    long.extend_from_slice(&[0xFF; 20]);
    assert_eq!(
        msg(&long),
        "str is not valid UTF-8 at byte 20 of 40: ff ff ff ff ff ff ff ff …"
    );

    // Exactly the window's worth, with nothing after it, is not marked.
    let exact = [&[0x02, 8][..], &[0xFF; 8]].concat();
    assert_eq!(
        msg(&exact),
        "str is not valid UTF-8 at byte 0 of 8: ff ff ff ff ff ff ff ff"
    );
}

/// `bytes` accepts every byte string `str` rejects — the UTF-8 rule is the
/// type's, not the format's.
#[test]
fn bytes_accepts_what_str_rejects() {
    for (name, bytes, expected) in BAD_VECTORS {
        if !matches!(expected, CellParseError::InvalidUtf8 { .. }) {
            continue;
        }
        // Same length byte and value, only the type id swapped.
        // `parse_prefix`, since one vector carries a deliberate trailing
        // byte that is a framing matter rather than a content one.
        let as_bytes = [&[CellType::Bytes.id()][..], &bytes[1..]].concat();
        let (cell, _) =
            CellValue::parse_prefix(&as_bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(cell.cell_type(), CellType::Bytes, "{name}");
        let declared = bytes[1] as usize;
        assert_eq!(cell.value(), &bytes[2..2 + declared], "{name}");
    }
}

/// A value too long to frame is rejected at construction — the wire form
/// has no way to express it, so there is no cell to parse.
#[test]
fn over_long_values_cannot_be_built() {
    for ty in [CellType::Str, CellType::Bytes] {
        assert_eq!(
            CellValue::new(ty, &[b'a'; MAX_VALUE_LEN + 1], false).unwrap_err(),
            CellParseError::TooLong {
                max: MAX_VALUE_LEN,
                actual: MAX_VALUE_LEN + 1
            },
            "{ty:?}"
        );
    }
    // Exactly at the maximum still works.
    assert!(CellValue::new(CellType::Bytes, &[0u8; MAX_VALUE_LEN], false).is_ok());
}

/// The point of the length byte: cells pack adjacently and a truncated cell
/// is rejected rather than read as a shorter valid value.
#[test]
fn packed_cells_walk_and_truncation_is_caught() {
    let seven = 7u64.to_be_bytes();
    let cells = [
        CellValue::new(CellType::Str, b"hi", true).unwrap(),
        CellValue::new(CellType::Uint(Width::W8), &seven, false).unwrap(),
        CellValue::new(CellType::Bytes, &[0xDE, 0xAD], false).unwrap(),
        CellValue::new(CellType::Tombstone, &[], false).unwrap(),
    ];

    let mut packed = Vec::new();
    for cell in &cells {
        cell.encode_into(&mut packed);
    }

    let mut rest = &packed[..];
    for expected in &cells {
        let (cell, tail) = CellValue::parse_prefix(rest).unwrap();
        assert_eq!(cell, *expected);
        rest = tail;
    }
    assert!(rest.is_empty());

    // Cutting the run anywhere never yields the same first cell followed by
    // a clean walk — the truncation is always caught.
    for cut in 1..packed.len() {
        let mut rest = &packed[..cut];
        let walked = std::iter::from_fn(|| match CellValue::parse_prefix(rest) {
            Ok((cell, tail)) => {
                rest = tail;
                Some(cell)
            }
            Err(_) => None,
        })
        .count();
        assert!(
            walked < cells.len() || !rest.is_empty(),
            "truncating at {cut} still walked the whole run"
        );
    }
}

#[test]
fn typed_accessors() {
    assert_eq!(
        CellValue::parse(&[0x02, 0x02, b'h', b'i'])
            .unwrap()
            .as_str(),
        Some("hi")
    );
    assert_eq!(CellValue::parse(&[0x01, 1]).unwrap().as_bool(), Some(true));
    assert_eq!(CellValue::parse(&[0x01, 0]).unwrap().as_bool(), Some(false));
    // Wrong type: no coercion, no panic.
    assert_eq!(CellValue::parse(&[0x01, 1]).unwrap().as_str(), None);
    assert_eq!(CellValue::parse(&[0x00]).unwrap().as_bool(), None);
}

#[cfg(feature = "custom_types")]
#[test]
fn custom_ids_are_the_top_block() {
    assert!(CustomTypeId::new(63).is_none());
    assert_eq!(CustomTypeId::new(64).unwrap().get(), 64);
    assert_eq!(CustomTypeId::new(127).unwrap().get(), 127);
    assert!(CustomTypeId::new(128).is_none());
    // A custom type is framed like any other variable-width one, so a cell
    // carrying it stays self-delimiting even though its content is opaque.
    assert_eq!(
        CellType::from_id(100).unwrap().layout(),
        ValueLayout::LengthPrefixed { max: MAX_VALUE_LEN }
    );
}

/// With the feature off the custom block decodes to nothing, and says so
/// with its own error rather than pretending the ids are reserved.
#[cfg(not(feature = "custom_types"))]
#[test]
fn custom_ids_are_rejected_without_the_feature() {
    for id in CUSTOM_TYPE_ID_BASE..TYPE_ID_SPACE {
        assert_eq!(
            CellType::from_id(id),
            Err(CellParseError::CustomTypesDisabled(id)),
            "id {id}"
        );
    }
    // The core block is untouched by the feature.
    assert_eq!(CellType::from_id(63), Err(CellParseError::ReservedType(63)));
    assert!(CellType::from_id(29).is_ok());
}
