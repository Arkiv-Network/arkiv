# Arkiv seed input format (JSONL)

Status: draft v1. The input a future `arkiv-cli seed build` consumes in place
of the `SeedSpec` templates. The template generator becomes one producer of
this format; any other process may produce it too.

## 1. Scope

- Describes the entities and the funded accounts of the block-0 state.
- Creates only. No updates, deletes or history. Every entity has
  `$createdAt` 0.
- Order is significant: entity ids and minting nonces follow the order of
  parts in the index and of lines in each part.

## 2. Layout

```
seed/
  seed.json           index: version, ordered parts, defaults
  entities/*.jsonl    entity lines
  accounts/*.jsonl    funded account lines (optional)
```

`seed.json`:

```json
{"version": 1, "chainId": 1337,
 "defaults": {"$creator": "0x…", "$expiresAt": "never", "$contentType": "application/octet-stream"},
 "entities": [{"path": "entities/0001.jsonl", "defaults": {"$creator": "0x…"}},
              {"path": "entities/0002.jsonl"}],
 "accounts": ["accounts/owners.jsonl"]}
```

- `chainId` must equal the genesis chain id; minted keys depend on it. A CLI
  flag may override it, and a mismatch with the genesis is an error.
- `entities` is ordered. Per-part `defaults` override the index `defaults`.
- `defaults` may name `$creator`, `$owner`, `$expiresAt`, `$contentType` and
  `$creationFlags`, spelled exactly as on an entity line.
- Parts are UTF-8 JSON Lines. Blank lines are ignored. A line may be any
  length; the reader streams line by line.
- stdin mode (`--input -`) reads entity lines only, with defaults from flags.
- Optional: parts named `*.zst` or `*.gz` are decompressed on read.

## 3. Entity line

One JSON object per line. Keys starting with `$` are system fields and use
the node's built-in annotation names. Every other key is a custom attribute.

| Field            | Required | Value                                              | Notes                                                    |
|------------------|----------|----------------------------------------------------|----------------------------------------------------------|
| `$key`           | no       | `0x` + 64 hex, or `seed:<label>`                   | see section 4                                            |
| `$creator`       | yes      | `0x` + 40 hex, or `seed:<label>`                   | may come from defaults or from `$owner`; never zero; see section 4 |
| `$owner`         | no       | `0x` + 40 hex, or `seed:<label>`                   | defaults to `$creator`; may differ from it; see section 4 |
| `$expiresAt`     | no       | block number (integer or decimal string) or `never` | absolute block, greater than 0; default never            |
| `$contentType`   | no       | string                                             | default from defaults, else `application/octet-stream`   |
| `$payload`       | no       | base64, standard alphabet, padded                  | at most 131072 bytes decoded; default empty              |
| `$payloadText`   | no       | UTF-8 string                                       | seed-only alternative to `$payload`; not both            |
| `$creationFlags` | no       | array of `readonly`, `permissionlessExtension`     | default none                                             |

- One of `$creator` or `$owner` must be present, on the line or through
  defaults. A missing one is copied from the other. When both are given they
  may differ.
- A differing `$owner` is applied as a transfer from the creator to the owner
  right after the create, in the same batch, so the stored record and the
  index equal what the node holds after those two operations.
- `$createdAt`, `$all` and any other `$name` are errors.

```json
{"$key": "seed:order-17", "$creator": "0x8943545177806ED17B9F23F0a21ee5948eCaa776",
 "$expiresAt": "never", "$contentType": "application/json", "$payload": "eyJpZCI6MTd9",
 "rank": "u64:12", "team": "str:red", "price": "dec:19.99",
 "customer": "key:seed:customer-4", "archived": "bool:false"}
```

## 4. Keys and addresses

Entity keys, resolved per line, first rule that applies wins:

1. `$key` is `0x…`: that key, verbatim.
2. `$key` is `seed:<label>`: keccak256 over the UTF-8 bytes of the string
   `key:seed:<label>`, tag and prefix included. A `key:seed:<label>`
   attribute value resolves to the same key, so a reference and the entity it
   points at agree by construction.
3. No `$key`: minted with the node's derivation from `chainId`, `$creator`,
   the creator's minting nonce and salt 0. The nonce is the number of minted
   entities of that creator earlier in seed order. Minting advances the
   creator's minting nonce on the system account; rules 1 and 2 do not.

Addresses, wherever one is expected (`$creator`, `$owner`, an `addr:`
attribute value, an account line's `address`):

- `0x…`: that address, verbatim.
- `seed:<label>`: the first 20 bytes of keccak256 over the UTF-8 bytes of
  the string `addr:seed:<label>`. The same label gives the same address
  everywhere, so an account line can fund a labelled creator.

A key produced twice, by any rule, is an error (existing entity). Labelled
keys and addresses depend neither on the chain id nor on the position in the
seed. A labelled address has no known private key: it can own and create
entities at genesis, but nothing can sign for it afterwards. Creators that
must transact during a test need addresses with keys the test holds.

## 5. Custom attributes

- Name: a valid attribute identifier under the node's rules, not starting
  with `$`. At most 32 attributes per entity. The seeder sorts them by name.
- Value: a JSON string of the form `tag:value`, split at the first colon.
  The tag names the attribute type with the query language's spelling; the
  rest of the string is the value, taken verbatim, and validated with the
  same rules the query crate applies to a typed literal of that tag.

| Tag       | Example                            | Rule                                                      |
|-----------|------------------------------------|-----------------------------------------------------------|
| `i32`     | `"i32:-7"`                         | signed 32-bit                                             |
| `u64`     | `"u64:12"`                         | decimal or `0x` hex                                       |
| `u256`    | `"u256:1000000000000000000"`       | decimal or `0x` hex                                       |
| `dec`     | `"dec:19.99"`                      | at most 18 fractional digits, no exponent                 |
| `str`     | `"str:red"`                        | the rest of the string, unquoted; at most 128 bytes of UTF-8 |
| `addr`    | `"addr:0x…"` or `"addr:seed:<label>"` | 40 hex, EIP-55 or uniform case; the `seed:` form is resolved as in section 4 |
| `bytes32` | `"bytes32:0x…"`                    | 64 hex                                                    |
| `key`     | `"key:0x…"` or `"key:seed:<label>"` | 64 hex; the `seed:` form is resolved as in rule 4.2     |
| `bool`    | `"bool:true"`, `"bool:false"`      | exactly `true` or `false`                                 |

- Tags are lowercase. The value is never trimmed: `"str: red"` carries the
  leading space, and `"str:"` is the empty string. Colons after the first
  belong to the value, which is what makes `key:seed:customer-4` work.
- An untagged string, a number, a JSON boolean, an object or an array is an
  error. There is no default type, so a value can never be silently misread.
- References are weak, as on chain: `key:seed:…` is not checked for
  existence and may dangle. The same holds for `addr:seed:…`.

## 6. Account line

```json
{"address": "0x…", "balance": "1000000000000000000"}
```

- `address` is `0x…` or `seed:<label>`, resolved as in section 4.
- `balance` in wei, decimal string or `0x` hex, greater than 0.
- Optional `nonce`, default 0. No code, storage or private keys.
- An address the seed also writes (entity, index or system account) is an
  error. A duplicate address is an error.

## 7. Validation

- `seed check` validates every rule above plus the protocol limits, part by
  part in parallel, without building. An optional `--refs` pass reports
  dangling `seed:` references and duplicate labels.
- `seed build` fails on the first invalid line, naming the part and the line.

## 8. Outputs

In addition to today's dump, `stateHash` genesis and manifest:

- `keys.jsonl`: one line per entity in seed order,
  `{"i": N, "key": "0x…", "creator": "0x…", "owner": "0x…", "label": "…"}`
  with `label` only for `seed:` keys.
- `addresses.jsonl`: one line per distinct address label seen,
  `{"label": "…", "address": "0x…"}`.
- Manifest additions: `format`, per-part SHA-256 and line counts, entity and
  account totals, each creator's minting nonce after genesis, the batch
  settings used.

## 9. Determinism

The state root is a pure function of the index order, the bytes of every
part, the chain id, and the batch settings whenever an attribute holds more
than 32 distinct values (range-index node splits depend on insertion order).
Keep all of them with the snapshot.

## 10. Operational notes

- Part size is an operational choice, 1 to 4 GB works well for parallel
  generation and validation. It has no memory impact.
- The builder batches by payload bytes, about 100 MB per batch, rather than
  by entity count, so peak memory stays flat for large payloads.
- Every distinct attribute value keeps one index bucket in the builder's
  cache until the end. Unique values, many creators or owners, and one-to-one
  `key` references grow memory linearly with the seed. Watch
  `cached_accounts`.
- Not expressible: history, updates or deletes, a creation block other
  than 0.
