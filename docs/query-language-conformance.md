# Query language conformance tracker

Where the implementation of the `arkiv_query` language departs from
[api-spec.md §4](api-spec.md#4-read-path--the-arkiv_query-language), and what
is left to do about it.

- **Baseline:** commit `7f81691` (reviewed 2026-10-26).
- **Code in scope:** the parser (`crates/arkiv-query`), the `Query` AST and its
  lowering to a store filter (`crates/arkiv-interfaces`), the GolemDB adapter
  (`crates/arkiv-golemdb`) and the RPC (`crates/arkiv-reth-rpc`).
- **Evidence:** "parser" results were obtained by running the query through
  `arkiv_query::parse`. "code" results come from reading the code path and the
  adapter's own unit tests, not from a live node.
- **Line numbers** are as of the last update to this file; the function names
  next to them are the stable reference.

Status values: `open`, `in progress`, `done`, `decision` (choose between
changing the code and changing the spec).

## Summary

| ID | Item | Layer | Priority | Status |
|---|---|---|---|---|
| [Q1](#q1--not-fails-on-golemdb) | `NOT` fails on GolemDB with an internal error | GolemDB adapter, RPC | P1 | open |
| [Q2](#q2--golemdb-treats-key-and-bytes32-as-one-type) | GolemDB treats `key` and `bytes32` as one type | GolemDB adapter | P1 | open |
| [Q3](#q3--range-operators-only-on-numeric-types) | Range operators only on numeric types | parser, store | P1 | done |
| [Q4](#q4---is-rejected) | `!=` is rejected | parser, AST | P2 | open |
| [Q5](#q5--between-is-not-implemented) | `BETWEEN` is not implemented | lexer, parser, AST, store | P2 | done |
| [Q6](#q6--untagged-quoted-strings-accepted-for-system-attributes) | Untagged quoted strings accepted for `$owner` `$creator` `$key` | parser | P3 | decision |
| [Q7](#q7--type-tags-are-case-insensitive) | Type tags are case-insensitive | lexer, parser | P3 | decision |
| [Q8](#q8--andor-nesting-capped-at-4-levels) | `AND`/`OR` nesting capped at 4 levels | parser limits | P3 | decision |
| [S1–S6](#spec-text-issues) | Problems in the spec text itself | spec | P2 | open |
| [D1–D2](#out-of-date-crate-docs) | Out-of-date crate docs | docs | P3 | in progress |

Suggested order: Q1 and Q2 first, since they affect every deployment. Q3 and
Q5 are done, and Q3 unblocks Q4.

---

## Q1 — `NOT` fails on GolemDB

**Status:** open · **Priority:** P1

**Spec:** `NOT (…)` is the full complement, evaluated against the
active-entity set.

**Code:** the parser builds `Query::Not` and the in-memory store evaluates it
correctly. The GolemDB adapter has no `NOT` and returns
`StoreError::NotImplemented`. The RPC reports any store error other than a
dropped state as a generic `-32603` internal error, not as one of the
query error codes.

GolemDB is the store in every real deployment: the dev image, Docker Compose
and Kurtosis all set `ARKIV_STORE_PATH`. The e2e `NOT` tests pass only because
the test harness defaults to the in-memory store
(`ARKIV_EXPERIMENTAL_STORE=memory`).

**Where:**
- [`node`, crates/arkiv-golemdb/src/convert/query.rs:125](../crates/arkiv-golemdb/src/convert/query.rs#L125)
- [`store_failure`, crates/arkiv-reth-rpc/src/lib.rs:200-205](../crates/arkiv-reth-rpc/src/lib.rs#L200-L205)
- [`spawn_process`, crates/arkiv-harness/src/node.rs:255-256](../crates/arkiv-harness/src/node.rs#L255-L256)
- e2e `NOT` tests on the in-memory store: [bin/arkiv-reth/tests/e2e.rs:635-655](../bin/arkiv-reth/tests/e2e.rs#L635-L655)

**Tasks:**
- [ ] Decide how GolemDB answers `NOT`: native support in GolemDB, or
      complementing against the live-entity set in the adapter.
- [ ] Implement it.
- [ ] Until it is supported, return a typed query error instead of `-32603`.
- [ ] Run the query e2e tests against GolemDB as well as the in-memory store.

**Done when:** `NOT (rank = u256(30))` on a GolemDB-backed node returns the
same entities as on the in-memory store.

---

## Q2 — GolemDB treats `key` and `bytes32` as one type

**Status:** open · **Priority:** P1

**Spec:** a value predicate asserts the exact type. `parent = key(X)` never
matches a `bytes32`-typed `parent`.

**Code:** GolemDB stores a `key` as the same 32 fixed bytes as a `bytes32`,
so the adapter cannot tell them apart:
- `parent = key(X)` also matches a `bytes32`-typed `parent` holding `X`, and
  the reverse. The same holds for the range comparisons Q3 enabled:
  `parent > key(X)` also matches `bytes32`-typed `parent` values above `X`.
- `typeof(x) = bytes32` also finds `key`-typed attributes.
- `typeof(x) = key` returns `NotImplemented`, which the RPC reports as
  `-32603`.

**Where:**
- [`leaf`, crates/arkiv-golemdb/src/convert/query.rs:139-153](../crates/arkiv-golemdb/src/convert/query.rs#L139-L153)
- Adapter test asserting the refusal: [crates/arkiv-golemdb/tests/query.rs:351-371](../crates/arkiv-golemdb/tests/query.rs#L351-L371)

**Tasks:**
- [ ] Give `key` its own type in GolemDB, or carry the Arkiv type alongside
      the value so the adapter can filter on it.
- [ ] Make `typeof(x) = key` answerable.
- [ ] Add a GolemDB test: an entity with a `bytes32` attribute and one with a
      `key` attribute holding the same bytes; each typed query finds only its
      own.

**Done when:** `key` and `bytes32` predicates never match each other's
attributes on GolemDB, and `typeof(x) = key` works.

---

## Q3 — Range operators only on numeric types

**Status:** done · **Priority:** P1

**Spec (operator × type matrix):** `< <= > >=` and `BETWEEN` work on every
type except `bytes`. Ordering is numeric for `i32` `u64` `u256` `dec`,
`false < true` for `bool`, and unsigned byte order for `str` (raw UTF-8, no
collation), `bytes32`, `addr` and `key`. The system-attribute table lists
`$key`, `$owner` and `$creator` as "all comparisons".

**Before:** range operators worked only on `i32`, `u64`, `u256` and `dec`.
The parser rejected them on `bool`, `str`, `bytes32`, `addr` and `key` with
`-32002 "… values have no ordering"`. The store's per-type index rules made
`bool`, `bytes32`, `addr` and `key` equality-only, and `str` equality and
prefix only.

**GolemDB check:** GolemDB (`golemdb-api` at `d910d14`) accepts range bounds
on every indexable cell type, which is every type except variable-length
`bytes` (`Bounds::try_new`). It runs a range as an ordered scan of the
attribute's index terms. A term is `name || 0x00 || type tag || value`, with
`bool`, unsigned, string and fixed-bytes values stored raw and strings not
length-prefixed, so byte order is the spec's order. A `bool` reaches GolemDB
as one byte, `0x00` or `0x01`. No adapter change was needed.

**Resolution:**
- Store: every indexed type is now `IndexClass::EqRange`, and `str` is
  `IndexClass::EqRangePrefix` (range and prefix). The in-memory, Valkey and
  GolemDB stores all apply these rules
  ([`TypeId::index_class`, crates/arkiv-interfaces/src/store.rs:258-268](../crates/arkiv-interfaces/src/store.rs#L258-L268),
  [`IndexClass`, crates/arkiv-interfaces/src/store.rs:270-293](../crates/arkiv-interfaces/src/store.rs#L270-L293)).
- Parser: `check_operator` is removed; every literal the language can write
  takes `< <= > >=`. `STARTSWITH` on a type other than `str` is still `-32002`.
- Docs: the `arkiv-query` crate and module docs, the `TYPE_ERROR` code docs
  and the store `TypeId` docs now describe the new rule.
- On GolemDB a `key` range also matches `bytes32` attributes of the same
  name; that is part of Q2.

**Tests:**
- Store conformance suite, run on the in-memory store and on Valkey:
  `query_range_follows_each_types_order` and
  `query_prefix_on_a_type_without_one_is_invalid` in
  [crates/arkiv-interfaces/src/store/conformance.rs](../crates/arkiv-interfaces/src/store/conformance.rs).
- GolemDB on disk, through the parser, including `$owner` and `$key`:
  `a_range_follows_the_order_of_every_type` in
  [crates/arkiv-golemdb/tests/query.rs](../crates/arkiv-golemdb/tests/query.rs).
- GolemDB adapter: `a_range_on_a_non_numeric_type_is_a_range_of_that_type` in
  [crates/arkiv-golemdb/src/convert/query.rs](../crates/arkiv-golemdb/src/convert/query.rs).
- Parser, including `$owner`, `$creator` and `$key`:
  `ranges_are_allowed_on_every_type` and
  `ranges_are_allowed_on_the_system_attributes` in
  [crates/arkiv-query/src/parse.rs](../crates/arkiv-query/src/parse.rs).
- Live node (in-memory store): `query_operator_classes_over_a_live_node` in
  [bin/arkiv-reth/tests/e2e.rs](../bin/arkiv-reth/tests/e2e.rs) asserts `str`
  and `$key` ranges. Its error list now checks `STARTSWITH` on a `u256`
  instead of a range on a `str`.

**Tasks:**
- [x] Store: allow range on `bool`, `bytes32`, `addr`, `key` and `str`
      (`str` keeps prefix too).
- [x] GolemDB adapter: confirm GolemDB accepts range bounds for these types.
- [x] Parser: accept every type the spec lists.
- [x] Tests: in-memory, Valkey and GolemDB range queries on each type.
- [x] Update the tests that asserted the old behavior.

**Done when:** every row of the operator × type matrix behaves as specified
on both stores.

---

## Q4 — `!=` is rejected

**Status:** open · **Priority:** P2 · **Depends on:** Q3 (done)

**Spec:** `!=` is typed value-negation: the attribute is set and indexed as
that type, and its value differs. It is listed for every type except `bytes`.

**Code:** the parser rejects it on purpose with
`-32002 "!= is not part of the query language"`. The AST has no variant for
it.

**Where:**
- [`Parser::parse_predicate`, crates/arkiv-query/src/parse.rs:267-270](../crates/arkiv-query/src/parse.rs#L267-L270)
- [`Query`, crates/arkiv-interfaces/src/query.rs:28-68](../crates/arkiv-interfaces/src/query.rs#L28-L68)
- e2e test asserting the rejection: [bin/arkiv-reth/tests/e2e.rs:676](../bin/arkiv-reth/tests/e2e.rs#L676)
- `cut_operators_name_their_replacement` in [crates/arkiv-query/src/parse.rs](../crates/arkiv-query/src/parse.rs)

**Suggested approach:** with Q3 done, `a != v` can be lowered to
`a < v OR a > v`. That is exactly the spec's typed value-negation and needs
no `NOT`, so GolemDB can run it.

**Tasks:**
- [ ] Decide between an AST variant and desugaring in the parser.
- [ ] Implement it.
- [ ] Tests: `!=` never matches an unset, unindexed or differently-typed
      attribute (unlike `NOT (a = v)`).
- [ ] Update the tests listed above.

**Done when:** `a != i32(3)` parses and matches exactly the entities with an
indexed `i32` `a` other than 3, on both stores.

---

## Q5 — `BETWEEN` is not implemented

**Status:** done · **Priority:** P2

**Spec:**
- `a BETWEEN lo AND hi` is inclusive at both ends, the same as
  `a >= lo AND a <= hi`.
- Both bounds must have the same type tag; mixed bounds are a type error
  (`-32002`).
- `lo > hi` matches nothing.
- The `AND` inside `BETWEEN` belongs to it:
  `a BETWEEN 1 AND 5 AND b = 2` is `(a BETWEEN 1 AND 5) AND b = 2`.
- `BETWEEN` is a reserved word.

**Before:** it wasn't a keyword in the lexer and had no AST variant.
`a BETWEEN i32(1) AND i32(5)` failed with `-32001 expected a comparison
operator`, and `between = true` was accepted as an attribute name.

**Resolution:**
- Lexer: `BETWEEN` is a case-insensitive keyword
  ([`keyword_token`, crates/arkiv-query/src/lexer.rs:423-445](../crates/arkiv-query/src/lexer.rs#L423-L445)).
- Parser: after the attribute, `parse_between` takes `value AND value` itself,
  so the inner `AND` never reaches the conjunction
  ([`Parser::parse_between`, crates/arkiv-query/src/parse.rs:281](../crates/arkiv-query/src/parse.rs#L281)).
  Bounds of two types are `-32002` at the high bound, and each bound is held
  to a system attribute's type as any value is. `between` is in the
  reserved-word list.
- AST: `Query::Between { key, bounds: ValueRange }`. `ValueRange` has private
  fields and a constructor that refuses two types, so mixed bounds cannot be
  represented
  ([`ValueRange`, crates/arkiv-interfaces/src/query.rs:108](../crates/arkiv-interfaces/src/query.rs#L108)).
- Store: one `Test::Between { type_id, low, high }`, both bounds included,
  rather than a pair of comparisons. The in-memory and Valkey stores evaluate
  it with the shared matcher; a type without a range is `InvalidQuery`
  ([`Test`, crates/arkiv-interfaces/src/store.rs:478](../crates/arkiv-interfaces/src/store.rs#L478)).
- GolemDB: one `Filter::Range` leaf with both sides `Included`. GolemDB takes
  inverted bounds as matching nothing, which is the spec's `lo > hi` rule. A
  sort takes its type from a `BETWEEN` as from a comparison
  ([`leaf`, crates/arkiv-golemdb/src/convert/query.rs:165](../crates/arkiv-golemdb/src/convert/query.rs#L165)).
- Docs: the `arkiv-query` crate docs and example, the parser's grammar
  summary, the lexer header and the `TYPE_ERROR` code docs.

**Tests:**
- Parser: `between_takes_two_bounds_of_one_type`, `between_owns_its_and`,
  `between_bounds_of_two_types_are_a_type_error`,
  `between_on_a_system_attribute_takes_its_type` and
  `between_needs_its_full_shape`; `between = true` in
  `keywords_and_type_names_cannot_be_attributes`; the spec's full worked
  example in `the_specs_example_query_parses`.
- Lowering: `between_is_one_test_with_both_bounds`,
  `between_includes_both_ends_and_orders_negatives` and
  `bounds_of_two_types_cannot_be_built` in
  [crates/arkiv-interfaces/src/query/filter.rs](../crates/arkiv-interfaces/src/query/filter.rs).
- Store conformance suite (in-memory and Valkey):
  `query_between_includes_both_bounds`.
- GolemDB adapter: `a_between_is_one_range_with_both_sides_included`, plus a
  `Between` case in the sort and invalid-query tests.
- GolemDB on disk, through the parser, queries and counts:
  `a_between_includes_both_bounds` in
  [crates/arkiv-golemdb/tests/query.rs](../crates/arkiv-golemdb/tests/query.rs).
- Live node (in-memory store): `query_operator_classes_over_a_live_node`
  asserts `BETWEEN` on `u256` and `str`, an inverted range, and the
  `-32002`/`-32001` errors for mixed bounds and `between = true`.

**Tasks:**
- [x] Lexer: add `BETWEEN` as a keyword.
- [x] Parser: `rangeTest`, with the inner `AND` taken by the predicate.
- [x] Type check: same tag on both bounds, else `-32002`.
- [x] AST variant (`Query::Between`), lowered to one store test and one
      GolemDB range.
- [x] `lo > hi` matches nothing on both stores, rather than erroring.
- [x] Reject `between` as an attribute name.
- [x] Use `BETWEEN` in `the_specs_example_query_parses`.

**Done when:** every `BETWEEN` row in the
[repro table](#repro-queries) gives the spec's result.

---

## Q6 — Untagged quoted strings accepted for system attributes

**Status:** decision · **Priority:** P3

**Spec:** every type other than `i32` must be explicitly tagged
(`$owner = addr(…)`), and the grammar's `value` has no bare string.

**Code:** `$owner = '0x…'`, `$creator = '0x…'` and `$key = '0x…'` are
accepted, for compatibility with how the JS SDK writes them.

**Where:** [`bare_str_value`, crates/arkiv-query/src/parse.rs:553-576](../crates/arkiv-query/src/parse.rs#L553-L576)

**Tasks:**
- [ ] Decide: document this in the spec, or remove it once the SDK sends the
      tagged form.

---

## Q7 — Type tags are case-insensitive

**Status:** decision · **Priority:** P3

**Spec:** keywords are case-insensitive and attribute names are
case-sensitive. The tags are listed in lowercase; their case-sensitivity isn't
stated (see S5).

**Code:**
- `I32(10)` and `typeof(x) = I32` are accepted.
- The reserved-name check uses the same case-insensitive match, so `Key`,
  `Str` and `KEY` are rejected as attribute names, even though `Key` ≠ `key`
  under case-sensitive names.

**Where:**
- [`TypeTag::from_name`, crates/arkiv-query/src/lexer.rs:42-57](../crates/arkiv-query/src/lexer.rs#L42-L57)
- [`validate_user_name`, crates/arkiv-query/src/parse.rs:657-674](../crates/arkiv-query/src/parse.rs#L657-L674)

**Tasks:**
- [ ] Decide whether tags are case-sensitive, and write it into the spec.
- [ ] If they are, match tags exactly and allow `Key`, `Str` and similar as
      names.

---

## Q8 — `AND`/`OR` nesting capped at 4 levels

**Status:** decision · **Priority:** P3

**Spec:** node-local limits are allowed (`-32004`: "query length / predicate
count / nesting / bitmap-op budget"), but no figures are given.

**Code:** a query nesting `AND`/`OR` five levels deep is refused with
`-32004`. The other limits are 8 KiB of query text, 64 predicates and 32
levels of parentheses or `NOT`.

**Where:** [`MAX_AND_OR_DEPTH`, crates/arkiv-query/src/limits.rs:32](../crates/arkiv-query/src/limits.rs#L32)

**Tasks:**
- [ ] Decide whether 4 is enough for the SDK's use cases.
- [ ] State the node's limits in the spec or in node docs, so clients know
      what to expect.

---

## Spec text issues

Problems in [api-spec.md §4](api-spec.md#4-read-path--the-arkiv_query-language)
itself, to fix in the spec.

- [ ] **S1.** The grammar says `prefixTest = attrRef , STARTSWITH , strLit`,
      a bare `'ab'`, but the examples use `str('ab')`. The parser accepts only
      `str('ab')`.
- [ ] **S2.** `typeTag` includes `bool`, so `taggedLit` allows `bool(true)`,
      which the §2 table forbids ("bare `true` / `false` (no wrapper)").
- [ ] **S3.** `numLit`, `literalBody`, `letter`, `digit` and `utf8Char` are
      used in the grammar but never defined.
- [ ] **S4.** The grammar section says type tags are "not globally reserved",
      but the attribute-names section bans them as names. The parser follows
      the stricter rule.
- [ ] **S5.** Whether type tags are case-sensitive isn't stated (see Q7).
- [ ] **S6.** §6 gives parse-error `data` as `{ position, expected, got }`.
      The node sends `{ position, message }`
      ([`query_error`, crates/arkiv-reth-rpc/src/error.rs:12-23](../crates/arkiv-reth-rpc/src/error.rs#L12-L23)).
      Align one with the other.

## Out-of-date crate docs

- [ ] **D1.** The crate docs in [crates/arkiv-query/src/lib.rs](../crates/arkiv-query/src/lib.rs)
      say `!=` is reserved. Accurate today, but they will need rewriting with
      Q4. (The ordering paragraph was rewritten with Q3.)
- [ ] **D2.** The `Token::Int` doc in [crates/arkiv-query/src/lexer.rs](../crates/arkiv-query/src/lexer.rs)
      says "System attributes only", which is no longer true. (The lexer
      header, which called `EXISTS` and `TYPEOF` "reserved but
      unimplemented", was rewritten with Q5.)

---

## Repro queries

Re-run these to check progress. "Spec" is what §4 requires; "Current" is the
behavior as of the last update to this file. Hex values are elided: `addr`
takes 40 hex digits, and `key` and `bytes32` take 64.

| Query | Spec | Current | Source | Item |
|---|---|---|---|---|
| `a BETWEEN i32(1) AND i32(5)` | OK | OK (was `-32001`) | tests | Q5 ✓ |
| `a BETWEEN 1 AND 5 AND b = 2` | OK, `(a BETWEEN 1 AND 5) AND b = 2` | as the spec (was `-32001`) | tests | Q5 ✓ |
| `a BETWEEN i32(1) AND u64(5)` | `-32002` | `-32002` (was `-32001`) | tests | Q5 ✓ |
| `a BETWEEN 5 AND 1` | matches nothing | matches nothing (was `-32001`) | tests | Q5 ✓ |
| `between = true` | error (reserved word) | `-32001` (was OK) | tests | Q5 ✓ |
| `a != i32(3)` | OK | `-32002` | parser | Q4 |
| `name >= str('a')` | OK | OK (was `-32002`) | tests | Q3 ✓ |
| `flag > false` | OK | OK (was `-32002`) | tests | Q3 ✓ |
| `$owner > addr(0x…)` | OK | OK (was `-32002`) | tests | Q3 ✓ |
| `$key >= key(0x…)` | OK | OK (was `-32002`) | tests | Q3 ✓ |
| `h < bytes32(0x…)` | OK | OK (was `-32002`) | tests | Q3 ✓ |
| `NOT (rank = u256(30))`, GolemDB node | complement | `-32603` | code | Q1 |
| `parent = key(X)` against a `bytes32` `parent` = X, GolemDB node | no match | match | code | Q2 |
| `typeof(x) = key`, GolemDB node | OK | `-32603` | code | Q2 |
| `$owner = '0x…'` | error (must be tagged) | OK | parser | Q6 |
| `level = I32(10)` | unspecified | OK | parser | Q7 |
| `Key = true` | OK (names are case-sensitive) | `-32001` | parser | Q7 |
| `a = 1 AND (b = 1 OR (c = 1 AND (d = 1 OR (e = 1 AND f = 1))))` | OK (no limit stated) | `-32004` | parser | Q8 |
| `desc STARTSWITH 'ab'` | OK per grammar | `-32002` | parser | S1 |

## Already conformant

Behavior that matches §4; keep it that way while fixing the items above.

- A bare boolean expression with no `WHERE`; `*` only as the whole query
  (`NOT *` and `(*)` are rejected).
- Precedence `NOT` > `AND` > `OR`; case-insensitive keywords; `--` comments.
- Untagged numbers are `i32`, including a leading `+`.
- Every literal rule in the §2 table: `i32` range, `u64` and `u256` in
  decimal or hex, `dec` with at most 18 places and no rounding, exactly 64 hex
  digits for `bytes32` and `key`, EIP-55 checks for `addr`, `str` with `''`
  escapes up to 128 bytes, no `bool(…)` wrapper.
- `STARTSWITH` only on `str`, as a raw byte prefix; on any other type it is
  an error.
- `< <= > >=` on every type except `bytes`, in the spec's order, on the
  in-memory, Valkey and GolemDB stores (Q3).
- `BETWEEN`, inclusive at both ends, with its own `AND`, one type for both
  bounds, and nothing matched when `lo > hi`; one GolemDB range (Q5).
- `exists` and `typeof` see only indexed user attributes and refuse `$` names.
- Names: `[A-Za-z][A-Za-z0-9_.\-]*`, at most 32 bytes, case-sensitive; `$`
  and `#` rejected.
- Exact-type, indexed-only matching (in-memory store; see Q2 for GolemDB).
- `$createdAt`, `$updatedAt` and `$creationFlags` are refused with a clear
  message.
