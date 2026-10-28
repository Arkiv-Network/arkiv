//! The spec's worked example, and batches that differ only in order.
//!
//! Each transaction gets a fresh [`GasMeter`]; an entity carries its units,
//! expiry and `$rate` from one operation to the next, as the executor will.
//! Expected values are derived by hand in the comments, from the rules in
//! `docs/tokenomics-iteration-1-plan.md`, not from this crate.

use arkiv_interfaces::primitives::BlockNumber;
use arkiv_tokenomics::{FeeSchedule, GasMeter, RateUpdate, storage_time};

const GWEI: u64 = 1_000_000_000;
const FEE_BASE: u64 = 10 * GWEI;

/// What the executor keeps of an entity between operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entity {
    units: u64,
    expires_at: BlockNumber,
    rate: u64,
}

fn schedule(capacity_rate: u64) -> FeeSchedule {
    FeeSchedule {
        capacity_rate,
        ..FeeSchedule {
            capacity_rate: 14,
            cell_byte_units: 1,
            index_byte_units: 1,
            c_cell: 64,
            c_idx: 64,
            max_lifetime: 10_000_000,
        }
    }
}

fn create(
    meter: &mut GasMeter,
    units: u64,
    expires_at: BlockNumber,
    block: BlockNumber,
    schedule: &FeeSchedule,
) -> Entity {
    assert!(storage_time::within_max_lifetime(
        expires_at,
        block,
        schedule.max_lifetime
    ));
    let bought = storage_time::create(units, expires_at, block).unwrap();
    meter.add_storage_time(bought).unwrap();
    let rate = RateUpdate {
        old_rate: 0,
        units_before: 0,
        units_after: units,
        old_expires_at: expires_at,
        new_expires_at: expires_at,
        block_number: block,
        fee_base: FEE_BASE,
        storage_time: bought,
    }
    .new_rate(schedule)
    .unwrap();
    Entity {
        units,
        expires_at,
        rate,
    }
}

/// A patch to `units`: growing or shrinking.
fn patch(
    meter: &mut GasMeter,
    entity: Entity,
    units: u64,
    block: BlockNumber,
    schedule: &FeeSchedule,
) -> Entity {
    let bought = storage_time::patch(entity.units, units, entity.expires_at, block).unwrap();
    meter.add_storage_time(bought).unwrap();
    let rate = RateUpdate {
        old_rate: entity.rate,
        units_before: entity.units,
        units_after: units,
        old_expires_at: entity.expires_at,
        new_expires_at: entity.expires_at,
        block_number: block,
        fee_base: FEE_BASE,
        storage_time: bought,
    }
    .new_rate(schedule)
    .unwrap();
    Entity {
        units,
        rate,
        ..entity
    }
}

fn extend(
    meter: &mut GasMeter,
    entity: Entity,
    expires_at: BlockNumber,
    block: BlockNumber,
    schedule: &FeeSchedule,
) -> Entity {
    assert!(storage_time::within_max_lifetime(
        expires_at,
        block,
        schedule.max_lifetime
    ));
    let bought = storage_time::extend(entity.units, entity.expires_at, expires_at).unwrap();
    meter.add_storage_time(bought).unwrap();
    let rate = RateUpdate {
        old_rate: entity.rate,
        units_before: entity.units,
        units_after: entity.units,
        old_expires_at: entity.expires_at,
        new_expires_at: expires_at,
        block_number: block,
        fee_base: FEE_BASE,
        storage_time: bought,
    }
    .new_rate(schedule)
    .unwrap();
    Entity {
        expires_at,
        rate,
        ..entity
    }
}

fn storage_gas(meter: &GasMeter, schedule: &FeeSchedule) -> u64 {
    meter.finish(0, 0, schedule).unwrap().storage_gas
}

/// A 1 KiB entity of 1,556 units, created at block 0 for 3,888,000 blocks,
/// grown by 1,024 units at 1,296,000, and extended by 3,888,000 at 2,592,000
/// after `capacity_rate` rose from 14 to 21. At 10 gwei a unit-block is
/// 140,000 wei, later 210,000.
#[test]
fn the_spec_example() {
    let (before, after) = (schedule(14), schedule(21));

    // ceil(1,556 × 3,888,000 × 14 / 10⁶) = ceil(84,696.19); $rate 140,000 × 1,556.
    let mut created = GasMeter::new();
    let entity = create(&mut created, 1_556, 3_888_000, 0, &before);
    assert_eq!(storage_gas(&created, &before), 84_697);
    assert_eq!(entity.rate, 217_840_000);
    assert_eq!(
        u128::from(storage_gas(&created, &before)) * u128::from(FEE_BASE),
        846_970_000_000_000,
        "the pool's share, as the spec's table has it"
    );

    // ceil(1,024 × 2,592,000 × 14 / 10⁶) = ceil(37,158.91); $rate + 140,000 × 1,024.
    let mut patched = GasMeter::new();
    let entity = patch(&mut patched, entity, 2_580, 1_296_000, &before);
    assert_eq!(storage_gas(&patched, &before), 37_159);
    assert_eq!(entity.rate, 361_200_000);

    // ceil(2,580 × 3,888,000 × 21 / 10⁶) = ceil(210,651.84);
    // $rate (361,200,000 × 1,296,000 + 210,000 × 2,580 × 3,888,000) / 5,184,000, exact.
    let mut extended = GasMeter::new();
    let entity = extend(&mut extended, entity, 7_776_000, 2_592_000, &after);
    assert_eq!(storage_gas(&extended, &after), 210_652);
    assert_eq!(entity.rate, 496_650_000);
}

/// 1,000 units created at block 0 until 1,000,000: $rate 140,000,000. The
/// batches below run at block 100,000, 900,000 blocks before expiry.
fn base() -> Entity {
    create(&mut GasMeter::new(), 1_000, 1_000_000, 0, &schedule(14))
}

/// At one price, a grow and a shrink cost the same in either order: each
/// pays for its growth, 500 units × 900,000 blocks, 6,300 gas, and both end
/// at the $rate 700 units carry, 98,000,000.
///
/// - Grow first: 1,500 units at 210,000,000, then 700/1,500 of it kept.
/// - Shrink first: 200/1,000 kept, 28,000,000, then 500 units bought on top.
///
/// The growth is bought even when the batch ends smaller: a single patch
/// straight to 700 units buys nothing and ends at the same $rate.
#[test]
fn a_grow_and_a_shrink_in_either_order() {
    let schedule = schedule(14);

    let mut grow_first = GasMeter::new();
    let grown = patch(&mut grow_first, base(), 1_500, 100_000, &schedule);
    assert_eq!(grown.rate, 210_000_000);
    let grow_then_shrink = patch(&mut grow_first, grown, 700, 100_000, &schedule);

    let mut shrink_first = GasMeter::new();
    let shrunk = patch(&mut shrink_first, base(), 200, 100_000, &schedule);
    assert_eq!(shrunk.rate, 28_000_000);
    let shrink_then_grow = patch(&mut shrink_first, shrunk, 700, 100_000, &schedule);

    assert_eq!(storage_gas(&grow_first, &schedule), 6_300);
    assert_eq!(storage_gas(&shrink_first, &schedule), 6_300);
    assert_eq!(grow_then_shrink.rate, 98_000_000);
    assert_eq!(shrink_then_grow.rate, 98_000_000);

    let mut direct = GasMeter::new();
    let shrunk_once = patch(&mut direct, base(), 700, 100_000, &schedule);
    assert_eq!(storage_gas(&direct, &schedule), 0);
    assert_eq!(shrunk_once.rate, 98_000_000);
}

/// An extend and a shrink differ by order: extending first buys the added
/// 500,000 blocks for 1,000 units, 7,000 gas, half of which the shrink then
/// gives up; shrinking first buys them for 500 units, 3,500 gas. Both end at
/// 70,000,000 per block, the rate 500 units carry at 140,000 wei.
///
/// - Extend first: (140,000,000 × 900,000 + 140,000 × 1,000 × 500,000) / 1,400,000
///   = 140,000,000, then 500/1,000 of it kept.
/// - Shrink first: 70,000,000, then (70,000,000 × 900,000 + 140,000 × 500 × 500,000)
///   / 1,400,000 = 70,000,000.
#[test]
fn an_extend_and_a_shrink_in_either_order() {
    let schedule = schedule(14);

    let mut extend_first = GasMeter::new();
    let extended = extend(&mut extend_first, base(), 1_500_000, 100_000, &schedule);
    assert_eq!(extended.rate, 140_000_000);
    let extend_then_shrink = patch(&mut extend_first, extended, 500, 100_000, &schedule);

    let mut shrink_first = GasMeter::new();
    let shrunk = patch(&mut shrink_first, base(), 500, 100_000, &schedule);
    let shrink_then_extend = extend(&mut shrink_first, shrunk, 1_500_000, 100_000, &schedule);

    assert_eq!(storage_gas(&extend_first, &schedule), 7_000);
    assert_eq!(storage_gas(&shrink_first, &schedule), 3_500);
    assert_eq!(extend_then_shrink.rate, 70_000_000);
    assert_eq!(shrink_then_extend.rate, 70_000_000);
    assert_eq!(
        extend_then_shrink, shrink_then_extend,
        "the same entity either way"
    );
}
