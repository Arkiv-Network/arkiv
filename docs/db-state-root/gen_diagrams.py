#!/usr/bin/env python3
"""Emit the deck's diagrams as Graphviz DOT files into the given directory.

One function per diagram. Run `build.sh` (or `nix build`) to render them.
"""

import sys
from pathlib import Path

OUT = Path(sys.argv[1] if len(sys.argv) > 1 else "diagrams")

FONT = 'fontname="Helvetica"'
HEAD = f"""
  graph [{FONT}, fontsize=12, bgcolor="transparent", nodesep=0.35, ranksep=0.45, pad=0.2];
  node  [{FONT}, fontsize=12, shape=box, style="rounded,filled", fillcolor="#f4f4f4", color="#555555"];
  edge  [{FONT}, fontsize=10, color="#555555", arrowsize=0.7];
"""

# Palette
OLD = '#f4f4f4'      # unchanged / plain
NEW = '#ffe9a8'      # newly written
SHARED = '#d5ecd4'   # shared between versions
ROOT = '#cfe2ff'     # a root
KV = '#e8e8ff'       # a key-value record
CODE = '#ffd8d8'     # account code
SLOT = '#ffe9a8'     # storage slot


def write(name: str, body: str, rankdir: str = "TB") -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    text = f'digraph "{name}" {{\n  rankdir={rankdir};{HEAD}{body}\n}}\n'
    (OUT / f"{name}.dot").write_text(text)


def rec(label: str) -> str:
    """A record-shaped node label: fields separated by `|`, `{{ }}` nests.

    Written with doubled braces so it reads the same inside and outside an
    f-string; they are collapsed here.
    """
    label = label.replace("{{", "{").replace("}}", "}")
    return f'shape=record, style="filled", label="{label}"'


# ----------------------------------------------------------------------------
# Part 1: background
# ----------------------------------------------------------------------------

def persistent_tree() -> None:
    """A search tree before and after inserting 55, by path copying."""
    write("persistent-tree", f"""
  subgraph cluster_v1 {{ label="version 1  (root R1)"; color="#999999"; style=dashed;
    R1 [label="50", fillcolor="{ROOT}"];
  }}
  subgraph cluster_v2 {{ label="version 2  (root R2): insert 55"; color="#999999"; style=dashed;
    R2 [label="50'", fillcolor="{NEW}"];
    N70b [label="70'", fillcolor="{NEW}"];
    N60b [label="60'", fillcolor="{NEW}"];
    N55 [label="55", fillcolor="{NEW}"];
  }}
  N30 [label="30", fillcolor="{SHARED}"];
  N70 [label="70", fillcolor="{SHARED}"];
  N20 [label="20", fillcolor="{SHARED}"];
  N40 [label="40", fillcolor="{SHARED}"];
  N60 [label="60", fillcolor="{SHARED}"];
  N80 [label="80", fillcolor="{SHARED}"];

  R1 -> N30; R1 -> N70;
  N30 -> N20; N30 -> N40;
  N70 -> N60; N70 -> N80;

  R2 -> N30 [color="#b8860b"]; R2 -> N70b [color="#b8860b"];
  N70b -> N60b [color="#b8860b"]; N70b -> N80 [color="#b8860b"];
  N60b -> N55 [color="#b8860b"];

  legend [shape=plaintext, label=<<table border="0" cellborder="0" cellspacing="2">
    <tr><td bgcolor="{SHARED}">  </td><td align="left">shared by both versions</td></tr>
    <tr><td bgcolor="{NEW}">  </td><td align="left">copied or created for version 2: one node per level</td></tr>
  </table>>];
""")


def merkle_tree() -> None:
    write("merkle-tree", f"""
  root [label="root = H(h01 ‖ h23)", fillcolor="{ROOT}"];
  h01 [label="h01 = H(h0 ‖ h1)"];
  h23 [label="h23 = H(h2 ‖ h3)"];
  h0 [label="h0 = H(d0)"]; h1 [label="h1 = H(d1)"];
  h2 [label="h2 = H(d2)"]; h3 [label="h3 = H(d3)"];
  d0 [label="d0", fillcolor="{KV}"]; d1 [label="d1", fillcolor="{KV}"];
  d2 [label="d2", fillcolor="{KV}"]; d3 [label="d3", fillcolor="{KV}"];
  root -> h01; root -> h23;
  h01 -> h0; h01 -> h1; h23 -> h2; h23 -> h3;
  h0 -> d0; h1 -> d1; h2 -> d2; h3 -> d3;
""")


def patricia_trie() -> None:
    """A plain trie next to its path-compressed (Patricia) form."""
    write("patricia-trie", f"""
  subgraph cluster_plain {{ label="plain trie: one node per character"; color="#999999"; style=dashed;
    p0 [label="•", fillcolor="{ROOT}"];
    pc [label="c"]; pa [label="a"]; pr [label="r", fillcolor="{KV}"]; pt [label="t", fillcolor="{KV}"];
    prt [label="t", fillcolor="{KV}"];
    pd [label="d"]; pdo [label="o"]; pdg [label="g", fillcolor="{KV}"];
    p0 -> pc -> pa; pa -> pr; pa -> pt; pr -> prt;
    p0 -> pd -> pdo -> pdg;
  }}
  subgraph cluster_patricia {{ label="Patricia trie: runs with one child are merged into the edge"; color="#999999"; style=dashed;
    q0 [label="•", fillcolor="{ROOT}"];
    qca [label="branch"];
    qr [label="r  (car)", fillcolor="{KV}"];
    qt [label="t  (cat)", fillcolor="{KV}"];
    qrt [label="t  (cart)", fillcolor="{KV}"];
    qdog [label="dog", fillcolor="{KV}"];
    q0 -> qca [label="‘ca’"];
    qca -> qr; qca -> qt; qr -> qrt;
    q0 -> qdog [label="‘dog’"];
  }}
  note [shape=plaintext, label="keys: car, cart, cat, dog\ndepth follows where keys diverge, not key length\nlookups, inserts and ordered walks work the same way"];
""", rankdir="TB")


def merkle_patricia() -> None:
    """An Ethereum-style trie: branch, extension and leaf nodes, keys as nibbles."""
    write("merkle-patricia", f"""
  root [label="branch\\nroot = keccak(rlp(node))", fillcolor="{ROOT}"];
  e [label="extension\\npath: a 7"];
  b2 [label="branch"];
  l1 [label="leaf\\npath: 1 2   value: v1", fillcolor="{KV}"];
  l2 [label="leaf\\npath: f 0   value: v2", fillcolor="{KV}"];
  l3 [label="leaf\\npath: 0 0 0 1   value: v3", fillcolor="{KV}"];
  root -> e [label="nibble 3"];
  root -> l3 [label="nibble c"];
  e -> b2;
  b2 -> l1 [label="nibble 0"];
  b2 -> l2 [label="nibble 9"];
  note [shape=plaintext, label="keys: 0x3a7012, 0x3a79f0, 0xc0001\\nevery child is referenced by the hash of its encoding\\n(or inlined when shorter than 32 bytes)"];
""")


def reth_tables() -> None:
    write("reth-tables", f"""
  hdr [{rec("{{ block header | {{ number | parentHash | <sr> stateRoot | receiptsRoot | … }} }}")}, fillcolor="{ROOT}"];
  acct [{rec("{{ PlainAccountState | {{ address → | nonce | balance | codeHash | }} }}")}, fillcolor="{KV}"];
  stor [{rec("{{ PlainStorageState | {{ (address, slot) → | value }} }}")}, fillcolor="{KV}"];
  code [{rec("{{ Bytecodes | {{ codeHash → | bytes }} }}")}, fillcolor="{KV}"];
  trie [label="account trie over keccak(address)\\nleaf = rlp(nonce, balance, storageRoot, codeHash)\\nstorageRoot = trie over keccak(slot)", fillcolor="{OLD}"];
  hdr:sr -> trie [label="commits to"];
  trie -> acct; trie -> stor; trie -> code [style=dashed, label="by codeHash"];
  note [shape=plaintext, label="MDBX tables, flat key → value (reth's own store).\\nThe trie is recomputed from the changed keys after every block."];
""", rankdir="LR")


# ----------------------------------------------------------------------------
# Part 2: before this PR
# ----------------------------------------------------------------------------

def before_entities() -> None:
    write("before-entities", f"""
  key [label="entity key (32 bytes)\\n0x8f3a…c2 ‖ …", fillcolor="{KV}"];
  addr [label="account address = key[0..20]\\n0x8f3a…c2"];
  acct [{rec("{{ account | {{ nonce = 1 | balance = 0 | codeHash = keccak(code) | storageRoot }} }}")}, fillcolor="{OLD}"];
  code [label="code = 0xFE 0x01 ‖ RLP(entity record)\\n(creator, owner, blocks, expiry, flags,\\ncontent type, payload, attributes)", fillcolor="{CODE}"];
  sys [{rec("{{ system account 0x44…46 | {{ slot keccak(’nonces’ ‖ owner) → next minting nonce | slot keccak(’arkiv.entity_count’) → next id | slot keccak(’arkiv.key2id’ ‖ key) → id + 1 | slot keccak(’arkiv.id2key’ ‖ id) → key }} }}")}, fillcolor="{SLOT}"];
  key -> addr [label="truncate"]; addr -> acct; acct -> code [label="codeHash"];
  key -> sys [style=dashed, label="bookkeeping"];
""")


def before_eq_index() -> None:
    write("before-eq-index", f"""
  subgraph cluster_e {{ label="entities and their dense ids"; color="#999999"; style=dashed;
    e0 [label="id 0: key k0\\nteam = red", fillcolor="{KV}"];
    e1 [label="id 1: key k1\\nteam = blue", fillcolor="{KV}"];
    e2 [label="id 2: key k2\\nteam = red", fillcolor="{KV}"];
  }}
  pr [label="pair account\\nkeccak(\\"arkiv.pair\\" ‖ team ‖ 0 ‖ str ‖ 0 ‖ red)[0..20]\\ncode = roaring bitmap {{0, 2}}", fillcolor="{CODE}"];
  pb [label="pair account\\nkeccak(\\"arkiv.pair\\" ‖ team ‖ 0 ‖ str ‖ 0 ‖ blue)[0..20]\\ncode = roaring bitmap {{1}}", fillcolor="{CODE}"];
  pa [label="$all bucket\\ncode = roaring bitmap {{0, 1, 2}}", fillcolor="{CODE}"];
  e0 -> pr; e2 -> pr; e1 -> pb;
  e0 -> pa; e1 -> pa; e2 -> pa;
  note [shape=plaintext, label="one account per (attribute, type, value)\\nits code is the serialized set of entity ids\\ncodeHash = keccak(bitmap bytes) is what the state root sees"];
""")


def before_range_index() -> None:
    write("before-range-index", f"""
  hdr [{rec("{{ B+ tree header account | keccak(’arkiv.ibth’ ‖ rank ‖ u256)[0..20] | {{ slot 0: root id ‖ next id ‖ magic }} }}")}, fillcolor="{SLOT}"];
  n0 [{rec("{{ node account (id 0, internal) | keccak(’arkiv.ibtn’ ‖ header ‖ 0)[0..20] | {{ slot 0: right sibling ‖ key count ‖ leaf bit | keys[0..] | child ids[0..] }} }}")}, fillcolor="{SLOT}"];
  n1 [{rec("{{ node account (id 1, leaf) | {{ meta | keys: 7, 42 | presence markers }} }}")}, fillcolor="{SLOT}"];
  n2 [{rec("{{ node account (id 2, leaf) | {{ meta | keys: 90, 300 | presence markers }} }}")}, fillcolor="{SLOT}"];
  hdr -> n0 [label="root id"]; n0 -> n1; n0 -> n2; n1 -> n2 [label="right sibling", style=dashed];
  cas [label="strings: a cascade of accounts,\\none level per 32-byte chunk of the value,\\neach with an enumeration list account", fillcolor="{OLD}"];
  note [shape=plaintext, label="an ordered structure built by hand inside unordered storage slots\\nit holds distinct values only, not entities"];
""")


def before_insert() -> None:
    write("before-insert", f"""
  op [label="create entity k with\\nteam = red, rank = 42", fillcolor="{KV}"];
  id [label="allocate dense id\\nread + bump entity_count slot\\nwrite key2id and id2key slots", fillcolor="{SLOT}"];
  rec_ [label="write entity account\\ncode = record", fillcolor="{CODE}"];
  p1 [label="pair(team, red): read bitmap,\\nset bit id, write code", fillcolor="{CODE}"];
  p2 [label="pair(rank, 42): read bitmap,\\nset bit id, write code", fillcolor="{CODE}"];
  p3 [label="$all, $owner, $creator, $key,\\n$expiration, $createdAtBlock, $contentType:\\nsame again, one account each", fillcolor="{CODE}"];
  bt [label="rank B+ tree: descend from header,\\ninsert value 42 into a leaf, maybe split\\n(several slots per touched node)", fillcolor="{SLOT}"];
  cs [label="team cascade: level-0 slot for \\"red\\",\\nenumeration list append", fillcolor="{SLOT}"];
  root [label="reth rehashes every touched account\\n(≈ 12 accounts, dozens of slots)\\ninto the state root", fillcolor="{ROOT}"];
  op -> id -> rec_ -> p1 -> p2 -> p3 -> bt -> cs -> root;
""")


def before_query() -> None:
    write("before-query", f"""
  q [label="rank >= 42 AND team = \\"red\\"", fillcolor="{KV}"];
  bt [label="scan the rank B+ tree from 42:\\nvalues 42, 90, 300", fillcolor="{SLOT}"];
  pb [label="read pair(rank, 42), pair(rank, 90), pair(rank, 300)\\nunion the bitmaps → {{0, 2, 5}}", fillcolor="{CODE}"];
  pr [label="read pair(team, red) → {{0, 2}}", fillcolor="{CODE}"];
  and [label="intersect → {{0, 2}}"];
  page [label="page: sort ids, take the largest first,\\ncursor = an id"];
  map [label="id2key slots: 0 → k0, 2 → k2", fillcolor="{SLOT}"];
  ent [label="entity accounts k0, k2: decode code", fillcolor="{CODE}"];
  q -> bt -> pb -> and; q -> pr -> and; and -> page -> map -> ent;
""")


# ----------------------------------------------------------------------------
# Part 3: after this PR
# ----------------------------------------------------------------------------

def after_overview() -> None:
    write("after-overview", f"""
  subgraph cluster_reth {{ label="in reth (Ethereum state, MDBX)"; color="#777777"; style="dashed,rounded"; bgcolor="#fafafa";
    hdr [label="block header\\nstateRoot", fillcolor="{ROOT}"];
    trie [label="Ethereum account trie\\nEOAs: balances and nonces"];
    anchor [{rec("{{ anchor account 0x61726b69…7421 (’arkiv-database-root!’) | {{ nonce = 1 | slot 0 = database root }} }}")}, fillcolor="{SLOT}"];
    hdr -> trie -> anchor;
  }}
  subgraph cluster_turso {{ label="in turso (the Arkiv node store, one table)"; color="#777777"; style="dashed,rounded"; bgcolor="#fafafa";
    top [label="top node\\nkeccak(rlp[entities root, nonces root, indexes root])", fillcolor="{ROOT}"];
    ent [label="entities trie\\nkey (32 B) → record", fillcolor="{KV}"];
    non [label="nonces trie\\nowner (20 B) → next nonce", fillcolor="{KV}"];
    idx [label="index-of-indexes\\nname ‖ 0x00 ‖ type → index root", fillcolor="{KV}"];
    i1 [label="index (team, str)\\nenc(value) ‖ key → ()", fillcolor="{KV}"];
    i2 [label="index (rank, u256)\\nenc(value) ‖ key → ()", fillcolor="{KV}"];
    store [{rec("{{ nodes table | {{ keccak(rlp(node)) → rlp(node) }} }}")}, fillcolor="{OLD}"];
    top -> ent; top -> non; top -> idx;
    idx -> i1; idx -> i2;
    {{ent non i1 i2}} -> store [style=dashed];
  }}
  anchor -> top [label="names, by hash"];
""")


def after_entities() -> None:
    write("after-entities", f"""
  root [label="entities root", fillcolor="{ROOT}"];
  b [label="branch"];
  l1 [label="leaf   path: rest of k1\\nvalue = RLP(record k1)", fillcolor="{KV}"];
  l2 [label="leaf   path: rest of k2\\nvalue = RLP(record k2)", fillcolor="{KV}"];
  e [label="extension   path: 8 f 3"];
  b2 [label="branch"];
  l3 [label="leaf   value = RLP(record k3)", fillcolor="{KV}"];
  l4 [label="leaf   value = RLP(record k4)", fillcolor="{KV}"];
  root -> b;
  b -> l1 [label="nibble 2"]; b -> l2 [label="nibble 7"]; b -> e [label="nibble 8"];
  e -> b2; b2 -> l3 [label="nibble a"]; b2 -> l4 [label="nibble c"];
  note [shape=plaintext, label="the entity key is already a keccak: used raw, no second hash, no truncation\\nthe record encoding is unchanged from the account-code layout\\nno id, no id maps"];
""")


def after_index() -> None:
    write("after-index", f"""
  ioi [label="index-of-indexes root", fillcolor="{ROOT}"];
  t [label="leaf  key: \\"team\\" ‖ 0x00 ‖ 8 (str)\\nvalue = root of the team index", fillcolor="{KV}"];
  r [label="leaf  key: \\"rank\\" ‖ 0x00 ‖ 4 (u256)\\nvalue = root of the rank index", fillcolor="{KV}"];
  ioi -> t; ioi -> r;

  troot [label="team index root", fillcolor="{ROOT}"];
  tb [label="branch"];
  eb [label="extension  \\"blue\\" 0x00 0x00"];
  lb [label="leaf  ‖ k1", fillcolor="{KV}"];
  er [label="extension  \\"red\\" 0x00 0x00"];
  rb [label="branch"];
  lr0 [label="leaf  ‖ k0", fillcolor="{KV}"];
  lr2 [label="leaf  ‖ k2", fillcolor="{KV}"];
  t -> troot [style=dashed];
  troot -> tb; tb -> eb [label="b"]; tb -> er [label="r"];
  eb -> lb; er -> rb; rb -> lr0 [label="k0[0]"]; rb -> lr2 [label="k2[0]"];
  note [shape=plaintext, label="key = order-preserving value ‖ entity key, value = a marker\\nequality: walk the subtree under enc(value)\\nSTARTSWITH: walk under the escaped prefix\\nrange: in-order walk between two bounds\\nentities with the same value are ascending by key"];
""")


def after_insert() -> None:
    write("after-insert", f"""
  subgraph cluster_old {{ label="root before"; color="#999999"; style=dashed;
    r1 [label="team index root", fillcolor="{ROOT}"];
    tb1 [label="branch", fillcolor="{SHARED}"];
    er1 [label="ext \\"red\\"", fillcolor="{SHARED}"];
  }}
  subgraph cluster_new {{ label="root after: insert k5 with team = red"; color="#999999"; style=dashed;
    r2 [label="team index root'", fillcolor="{NEW}"];
    tb2 [label="branch'", fillcolor="{NEW}"];
    er2 [label="ext \\"red\\"'", fillcolor="{NEW}"];
    rb2 [label="branch'", fillcolor="{NEW}"];
    l5 [label="leaf ‖ k5", fillcolor="{NEW}"];
  }}
  eb [label="ext \\"blue\\" → leaf ‖ k1", fillcolor="{SHARED}"];
  rb [label="branch", fillcolor="{SHARED}"];
  l0 [label="leaf ‖ k0", fillcolor="{SHARED}"]; l2 [label="leaf ‖ k2", fillcolor="{SHARED}"];
  r1 -> tb1; tb1 -> eb; tb1 -> er1; er1 -> rb; rb -> l0; rb -> l2;
  r2 -> tb2 [color="#b8860b"]; tb2 -> eb [color="#b8860b"]; tb2 -> er2 [color="#b8860b"];
  er2 -> rb2 [color="#b8860b"]; rb2 -> l0 [color="#b8860b"]; rb2 -> l2 [color="#b8860b"]; rb2 -> l5 [color="#b8860b"];
  note [shape=plaintext, label="new nodes: one per level on the changed path (≈ log n)\\neverything else is shared with the old root, which stays readable\\nthe old nodes are never modified or deleted"];
""")


def after_commit() -> None:
    write("after-commit", f"""
  tx [label="transaction: create k5\\n(team = red, rank = 42)", fillcolor="{KV}"];
  read [label="read parent root from the anchor slot\\nthrough the revm Database"];
  ent [label="entities trie: insert k5 → record\\n(path copy)", fillcolor="{NEW}"];
  i1 [label="team index: insert enc(red) ‖ k5", fillcolor="{NEW}"];
  i2 [label="rank index: insert enc(42) ‖ k5", fillcolor="{NEW}"];
  i3 [label="$owner, $creator, $key, $expiration,\\n$createdAtBlock, $contentType indexes: same", fillcolor="{NEW}"];
  ioi [label="index-of-indexes: update the changed roots", fillcolor="{NEW}"];
  non [label="nonces trie: owner → nonce + 1", fillcolor="{NEW}"];
  top [label="top node → new database root", fillcolor="{ROOT}"];
  flush [label="flush the new nodes to the node store\\n(durable, before the block can be committed)"];
  diff [label="Ethereum diff: sender balance and nonce,\\nanchor slot 0 = new database root", fillcolor="{SLOT}"];
  tx -> read -> ent -> i1 -> i2 -> i3 -> ioi -> non -> top -> flush -> diff;
  note [shape=plaintext, label="reth rehashes 2 accounts and 1 slot, whatever the entity looks like"];
""")


def after_query() -> None:
    write("after-query", f"""
  q [label="rank >= 42 AND team = \\"red\\"", fillcolor="{KV}"];
  a [label="anchor slot at the requested block → database root", fillcolor="{SLOT}"];
  r [label="rank index: in-order walk from enc(42)\\n→ k0, k2, k5 (keys straight from the index keys)", fillcolor="{KV}"];
  t [label="team index: walk under enc(\\"red\\")\\n→ k0, k2", fillcolor="{KV}"];
  and [label="sorted-merge intersection → k0, k2"];
  page [label="page: skip the cursor offset, take n\\ncursor = an offset, stable at one root"];
  ent [label="entities trie: k0, k2 → records", fillcolor="{KV}"];
  q -> a; a -> r; a -> t; r -> and; t -> and; and -> page -> ent;
  note [shape=plaintext, label="no id maps, no bitmaps, no value-to-entity resolution step\\nAND / OR / NOT are merges of sorted key streams"];
""")


def after_history() -> None:
    write("after-history", f"""
  bN [label="block N\\nanchor slot = root N", fillcolor="{SLOT}"];
  bN1 [label="block N+1\\nanchor slot = root N+1", fillcolor="{SLOT}"];
  rN [label="root N", fillcolor="{ROOT}"];
  rN1 [label="root N+1", fillcolor="{NEW}"];
  s1 [label="shared subtree", fillcolor="{SHARED}"];
  s2 [label="shared subtree", fillcolor="{SHARED}"];
  c [label="nodes changed in N+1", fillcolor="{NEW}"];
  bN -> rN; bN1 -> rN1;
  rN -> s1; rN -> s2; rN1 -> s1; rN1 -> c; c -> s2 [style=dashed];
  note [shape=plaintext, label="a historical query at block N opens root N\\nreorgs need nothing: the canonical anchor names the canonical root\\nunreachable roots are garbage, never a gap"];
""", rankdir="LR")


def before_after_table() -> None:
    write("before-after", f"""
  t [shape=plaintext, label=<<table border="0" cellborder="1" cellspacing="0" cellpadding="6">
    <tr><td bgcolor="#dddddd"></td><td bgcolor="#dddddd"><b>before</b></td><td bgcolor="#dddddd"><b>after</b></td></tr>
    <tr><td align="left">entity</td><td align="left">an account, record in code, key truncated to 20 bytes</td><td align="left">a leaf in the entities trie, full 32-byte key</td></tr>
    <tr><td align="left">entity identity in the index</td><td align="left">dense u64 id, two maps in the system account</td><td align="left">the key itself</td></tr>
    <tr><td align="left">equality index</td><td align="left">one account per (attr, type, value), roaring bitmap in code</td><td align="left">a prefix of one trie per (attr, type)</td></tr>
    <tr><td align="left">range index</td><td align="left">hand-built B+ tree in storage slots, string cascade</td><td align="left">the same trie, walked in order</td></tr>
    <tr><td align="left">state root cost per write</td><td align="left">≈ 12 accounts and dozens of slots rehashed by reth</td><td align="left">≈ log n trie nodes per changed key, 1 slot in reth</td></tr>
    <tr><td align="left">history and reorgs</td><td align="left">reth's changesets and unwind</td><td align="left">old roots stay readable; nothing to unwind</td></tr>
    <tr><td align="left">commitment</td><td align="left">reth's state root, indirectly</td><td align="left">the database root in the anchor slot, verifiable with eth_getProof</td></tr>
  </table>>];
""")


if __name__ == "__main__":
    for fn in [
        persistent_tree, merkle_tree, patricia_trie, merkle_patricia, reth_tables,
        before_entities, before_eq_index, before_range_index, before_insert, before_query,
        after_overview, after_entities, after_index, after_insert, after_commit, after_query,
        after_history, before_after_table,
    ]:
        fn()
    print(f"wrote {len(list(OUT.glob('*.dot')))} diagrams to {OUT}")
