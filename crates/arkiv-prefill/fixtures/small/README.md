# Prefill fixture: small

A complete prefill, small enough to read: the example genesis file, two archives and their set file.

| Archive | Parts | Entities |
|---|---|---|
| `A0` | `entities-0001.jsonl` | `0xa1…`: text payload and content type · `0xa2…`: created by Alice, owned by Bob · `0xa3…`: read-only, with an entity-key attribute |
| `A1` | `entities-0001.jsonl`, `entities-0002.jsonl` | `0xb1…`: expiry far past `max_lifetime` · `0xb2…`: permissionless extension, `bytes32` attribute · `0xb3…`: in a second part |

`expected.json` holds block 0's state root and hash after prefilling and finalizing this fixture. Both depend only on these files: any machine, any run gives the same values.

Two checks use it:

- `just prefill_check` runs a built `arkiv-reth prefill` on a copy, then `--finalize`, and compares the result with `expected.json`. CI runs it after the build.
- The `prefill` e2e test starts a node on the prefilled store and reads the entities at block 0.

If a change is meant to alter block 0 (GolemDB's encoding, the executor's writes, the archive format), regenerate the file and commit it:

```bash
just prefill_fixture_update
```
