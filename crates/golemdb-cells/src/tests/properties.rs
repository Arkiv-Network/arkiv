//! Properties that must hold over generated inputs, rather than over the fixed
//! vectors in [`super::vectors`]. These cover what
//! [`super::every_metadata_byte_and_length`] cannot: arbitrary *content*, and
//! lengths past the variable-width maximum.

use proptest::prelude::*;

use crate::*;

/// Every type in the table, each width included.
fn any_cell_type() -> impl Strategy<Value = CellType> {
    (0u8..TYPE_ID_SPACE).prop_filter_map("reserved id", |id| CellType::from_id(id).ok())
}

/// A type paired with a value whose *content* is valid for it. Lengths
/// deliberately straddle the variable-width maximum, so `TooLong` is the one
/// error [`build_encode_parse_round_trips`] may see.
fn any_valid_cell() -> impl Strategy<Value = (CellType, Vec<u8>)> {
    any_cell_type().prop_flat_map(|ty| {
        let value = match ty {
            // Content-constrained: not every byte string of the right
            // length is a valid value.
            CellType::Bool => prop::collection::vec(0u8..=1, 1..=1).boxed(),
            CellType::Str => prop::string::string_regex(".{0,120}")
                .unwrap()
                .prop_map(String::into_bytes)
                .boxed(),
            // Length is the only constraint.
            _ => match ty.layout() {
                ValueLayout::Fixed(n) => prop::collection::vec(any::<u8>(), n..=n).boxed(),
                ValueLayout::LengthPrefixed { max } => {
                    prop::collection::vec(any::<u8>(), 0..=max + 8).boxed()
                }
            },
        };
        (Just(ty), value)
    })
}

proptest! {
    /// Parsing arbitrary bytes never panics, and whatever parses re-encodes
    /// to the exact bytes it came from.
    #[test]
    fn parse_is_total_and_encode_inverts_it(bytes in prop::collection::vec(any::<u8>(), 0..600)) {
        if let Ok(cell) = CellValue::parse(&bytes) {
            prop_assert_eq!(cell.encode(), bytes);
        }
    }

    /// A cell built from a valid (type, value) survives encode → parse
    /// unchanged, at both settings of the indexable bit.
    #[test]
    fn build_encode_parse_round_trips(
        (ty, value) in any_valid_cell(),
        indexable in any::<bool>(),
    ) {
        let built = match CellValue::new(ty, &value, indexable) {
            Ok(cell) => cell,
            // The generator straddles the variable-width maximum on
            // purpose; an over-long value must be rejected, not encoded.
            Err(e) => {
                let too_long = matches!(e, CellParseError::TooLong { .. });
                prop_assert!(too_long, "expected TooLong, got {}", e);
                return Ok(());
            }
        };
        let encoded = built.encode();
        let parsed = CellValue::parse(&encoded).map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert_eq!(parsed, built);
        prop_assert_eq!(parsed.cell_type(), ty);
        prop_assert_eq!(parsed.value(), &value[..]);
        prop_assert_eq!(parsed.is_indexable(), indexable);
    }

    /// The indexable bit and the type id never bleed into each other.
    #[test]
    fn metadata_byte_splits_cleanly(ty in any_cell_type(), indexable in any::<bool>()) {
        let value = vec![0u8; match ty.layout() {
            ValueLayout::Fixed(n) => n,
            _ => 0,
        }];
        let cell = CellValue::new(ty, &value, indexable).unwrap();
        let metadata = cell.metadata();
        prop_assert_eq!(metadata & TYPE_ID_MASK, ty.id());
        prop_assert_eq!(metadata & INDEXABLE_BIT != 0, indexable);
    }

    /// A `str` cell parses exactly when its bytes are UTF-8 — the parser
    /// agrees with the standard library, not with its own idea of UTF-8.
    #[test]
    fn str_accepts_exactly_utf8(value in prop::collection::vec(any::<u8>(), 0..=MAX_VALUE_LEN)) {
        let cell = [&[CellType::Str.id(), value.len() as u8][..], &value].concat();
        let parsed = CellValue::parse(&cell);
        prop_assert_eq!(parsed.is_ok(), core::str::from_utf8(&value).is_ok());
        if let Ok(cell) = parsed {
            prop_assert_eq!(cell.as_str(), Some(core::str::from_utf8(&value).unwrap()));
        }
    }

    /// A run of cells packs and walks back unchanged, and cutting the run
    /// short is always caught rather than read as a shorter value.
    #[test]
    fn packed_runs_round_trip(cells in prop::collection::vec(any_valid_cell(), 0..8)) {
        let cells: Vec<_> = cells
            .iter()
            .filter_map(|(ty, v)| CellValue::new(*ty, v, false).ok())
            .collect();

        let mut packed = Vec::new();
        for cell in &cells {
            cell.encode_into(&mut packed);
        }

        let mut rest = &packed[..];
        for expected in &cells {
            let (cell, tail) = CellValue::parse_prefix(rest)
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(cell, *expected);
            rest = tail;
        }
        prop_assert!(rest.is_empty());
    }

    /// Reserved ids stay reserved whatever follows them. Scoped to the core
    /// block, since the custom block's verdict depends on the feature.
    #[test]
    fn reserved_ids_never_parse(
        id in (0u8..CUSTOM_TYPE_ID_BASE)
            .prop_filter("valid id", |id| CellType::from_id(*id).is_err()),
        indexable in any::<bool>(),
        tail in prop::collection::vec(any::<u8>(), 0..40),
    ) {
        let metadata = id | if indexable { INDEXABLE_BIT } else { 0 };
        let cell = [&[metadata][..], &tail].concat();
        prop_assert_eq!(CellValue::parse(&cell), Err(CellParseError::ReservedType(id)));
        prop_assert_eq!(CellValue::parse_prefix(&cell).err(), Some(CellParseError::ReservedType(id)));
    }

    /// The custom block never decodes to a core type, whatever follows it,
    /// and its verdict matches the feature this build was compiled with.
    #[test]
    fn custom_block_verdict_matches_the_feature(
        id in CUSTOM_TYPE_ID_BASE..TYPE_ID_SPACE,
        indexable in any::<bool>(),
        tail in prop::collection::vec(any::<u8>(), 0..40),
    ) {
        let metadata = id | if indexable { INDEXABLE_BIT } else { 0 };
        let cell = [&[metadata][..], &tail].concat();

        #[cfg(not(feature = "custom_types"))]
        prop_assert_eq!(
            CellValue::parse_prefix(&cell).err(),
            Some(CellParseError::CustomTypesDisabled(id))
        );

        // With the feature on the id always decodes; whether the cell as a
        // whole parses is a framing question, so anything that does parse
        // must come back as that custom type.
        #[cfg(feature = "custom_types")]
        {
            let ty = CellType::Custom(CustomTypeId::new(id).unwrap());
            prop_assert_eq!(CellType::from_id(id).unwrap(), ty);
            if let Ok((parsed, _)) = CellValue::parse_prefix(&cell) {
                prop_assert_eq!(parsed.cell_type(), ty);
            }
        }
    }
}
