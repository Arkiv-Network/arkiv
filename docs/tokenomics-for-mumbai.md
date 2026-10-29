# Tokenomics for Mumbai

Pre-read for the meeting on PR #230, 2026-10-29. The PR is blocked from merging: it is too large and too late for the review capacity we have. 

This note sets out what
the PR does right, what is actually in it, and a much smaller scope that still gives Mumbai
working fees. Detailed numbers and the reading order are in
[the review guide](pr-230-review-guide.md).

## What the PR gets right

- **One fee, split by cause.** Compute, storage and purge gas add up to the usual `gasUsed`,
  charged with EIP-1559 as before. Estimates include all three.
- **Storage is bought for a lifetime.** A create pays size × blocks, an extend pays for the
  added blocks, a patch pays for its growth. No refunds. Lifetimes are capped at six months.
- **The storage share goes to a pool** at `ARKIV_ADDRESS` instead of being burned.
- **Parameters from the genesis file.** Arkiv's storage parameters and GolemDB's own
  weights in one metering model.
- **All or nothing per transaction**, and user input never fails a block: an overflow
  becomes an out-of-gas revert, an op on a key that holds no entity reverts.
- **The accounting is detailed enough to find real problems.** It showed that extend and
  transfer rewrote every cell of an entity, so a 100 KiB entity paid about 100,000 extra gas.
  Fixed for extend and transfer; still open for patch.
- **Measured end to end through the SDK.** With first-guess weights, a block holds 285 mixed
  ops (creates, patches, extends) at 85% of the gas limit.

## The problem: size and timing

- **+13,248 / −623 lines in 92 files across 12 crates**, arriving shortly before Mumbai.
- **Review capacity is limited.** golemdb-base was accepted after fairly light testing. For
  fees the bar has to be higher: a pricing bug is a consensus bug and an attack surface.
- **The PR has grown since its approval.** It now also carries the GolemDB pricing from the
  genesis file (#250) and three follow-up fixes.
- **Even the production code alone is too much.** About 2,000 lines of consensus code,
  spread over many crates, is more than we can review properly in the time left.

## What is in the 13,000 lines

Test modules inside `src/`, the conformance suites and the test store count as tests.

| Part | Added | Removed | Share of added |
| --- | ---: | ---: | ---: |
| **Production code** | **2,051** | **424** | **15%** |
| Tests | 7,833 | 163 | 59% |
| Docs (specs, plan, guides) | 2,547 | 17 | 19% |
| SDK experiment script, dev container | 626 | 3 | 5% |
| Config (Cargo, genesis files) | 191 | 16 | 1% |

Production code by group:

| Group | Added | Removed |
| --- | ---: | ---: |
| Iteration 1, as first reviewed | 2,104 | 374 |
| GolemDB pricing (#250) | 381 | 322 |
| Follow-up fixes | 208 | 21 |

Production code by crate:

| Crate | Added | Removed |
| --- | ---: | ---: |
| `arkiv-interfaces` | 584 | 40 |
| `arkiv-tokenomics` (new) | 500 | 0 |
| `arkiv-reth-executor` | 341 | 221 |
| GolemDB, Valkey, state layer | 482 | 136 |
| others | 144 | 27 |

So the review load is about 2,000 lines of production code, not 13,000. That is still too
much for the time we have, but it shows where a cut has effect: in the production code, not
in the tests or docs.

## Proposal: a much smaller tokenomics for Mumbai

Keep the rules users see, because they are hard to change later. Make everything behind them
simple, because prices are easy to change later.

**Keep for Mumbai:**

- the fee split: compute plus storage in one `gasUsed`;
- storage = size × lifetime × rate; extends and growing patches pay for what they add; no
  refunds;
- the six-month lifetime cap.

**Cut for Mumbai:**

- **Parameters as constants in code:** no genesis section, no metering model, no pricing
  snapshot per block.
- **A fixed compute cost per Arkiv op** (create, patch, extend, transfer, delete), plus a
  per-byte and a per-indexed-attribute term. `PlaceholderCost` on `main` already has this
  shape; only its numbers change.
- **Size from the entity itself** (attribute bytes plus a constant per attribute), not from
  GolemDB's counters. That removes the receipt bookkeeping through the store and state
  layers.
- **No GolemDB engine metering in fees yet;** #250 waits.
- **No pool bookkeeping:** the storage share is burned with the rest of the base fee, like
  all gas on `main` today. Nothing is credited to `ARKIV_ADDRESS`, and value sent with a
  call is handled as on `main`. The first version pays nothing out of the pool anyway.
- **Docs, load test and SDK script** go to separate PRs.

**Estimated size:** a few hundred lines of production code plus focused tests, which one
reviewer can go through in a day. This is an estimate from the current code, not yet a
measured number.

**Trade-offs:** fixed costs only approximate the real database work; the per-byte and
per-attribute terms bound abuse through large ops. Changing a constant needs a node release.
Compute does not follow database growth.

**After Mumbai:** replace the fixed costs with GolemDB's metering and add the pool, as a
stack of small PRs (pricing crate, GolemDB pricing, executor integration, pool). The fee
structure stays the same, so for users this is a price change, not a change of rules; the
pool changes only where the storage share goes, not what a user pays. The current branch
supplies the code and the measured numbers.

What users would pay, measured with first-guess weights, a base fee of 10 gwei and GLM at
$0.10. These numbers can seed the constants:

| Op | Gas | USD |
| --- | ---: | ---: |
| Small entity, 100 blocks | ~86k | $0.00009 |
| Normal entity (10 attributes, 300 B), 1 week | ~163k | $0.00016 |
| Large entity (20 attributes, 100 KiB), 6 months | ~15.9M | $0.016 |
| Patch of one indexed attribute | ~51k | $0.00005 |
| Transfer | ~40k | $0.00004 |

### To decide in the meeting

1. Do we agree on the keep and cut lists above?
2. Who sets the initial constants, starting from the measured numbers?
3. Who writes the smaller PR, who reviews it, and by when?
4. What happens to #230 and #250 until after Mumbai: parked, or split into a stack now?
5. Which non-fee fixes in #230 does Mumbai need anyway (all-or-nothing transactions,
   reserved keys, the Valkey frame leak, the write cost of extend and transfer), and should
   they go in separately?
