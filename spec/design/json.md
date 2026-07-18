# JSON types — design

> The two PostgreSQL document types: **`json`** (validated, stored verbatim as text) and
> **`jsonb`** (parsed, canonicalized, stored in a compact decomposed binary form). Numbers
> are exact (`decimal`, never binary float); strings are UTF-8 `text`; `jsonb` objects keep
> their keys in a canonical sorted order with duplicates resolved last-wins. `json` is
> **not** comparable (matching PG — no btree/hash opclass); `jsonb` has PostgreSQL's total
> btree order and is orderable/groupable. Both ride the existing large-value
> overflow-chain + LZ4 path for big documents. This doc is the contract all three cores
> implement in lockstep (CLAUDE.md §2); the type rows are data in
> [../types/scalars.toml](../types/scalars.toml) and [../types/casts.toml](../types/casts.toml),
> the byte layout in [../fileformat/format.md](../fileformat/format.md), the
> SQL/JSON **path** language in [jsonpath.md](jsonpath.md), the **function/operator**
> surface in [json-sql-functions.md](json-sql-functions.md), and `JSON_TABLE` +
> record-returning functions in [json-table.md](json-table.md). PostgreSQL semantics are the
> default (CLAUDE.md §1), pinned against the live `postgres:18` oracle.

> **Status: SPEC-FIRST (design ratified, implementation pending).** This document, together
> with [jsonpath.md](jsonpath.md) / [json-sql-functions.md](json-sql-functions.md) /
> [json-table.md](json-table.md), is the paper design that the J-/P-/S-/B-/R-/T-series
> vertical slices (§12) implement. No core code or golden fixtures land with this doc; the
> data-file rows it specifies (the `scalars.toml`/`casts.toml`/`catalog.toml`/`registry.toml`/
> `manifest.toml` entries) are committed **with their slice**, byte-pinned against real core
> output, following the range precedent (`ranges.toml` + the `RANGES` table landed with R0's
> code, not as a doc-only change).

---

## 1. Surface

```sql
CREATE TABLE docs (id i32 PRIMARY KEY, body jsonb, raw json)
INSERT INTO docs VALUES (1, '{"a": 1, "tags": ["x", "y"]}', '{ "a" : 1 }')
SELECT id, body -> 'a', body ->> 'a', body #> '{tags,0}' FROM docs
SELECT * FROM docs WHERE body @> '{"a": 1}' AND body ? 'tags'
SELECT id, body FROM docs ORDER BY body              -- jsonb is orderable
SELECT jsonb_build_object('id', id, 'doc', body) FROM docs
```

- **Two types.** `json` and `jsonb` (no aliases; PG uses these exact names). Both are
  **storable** column types from their first storage slice (§12: J1 / J1b).
- **`json` preserves the input text.** `'{ "a" : 1 }'::json` validates well-formedness and
  stores the bytes **verbatim** — whitespace, key order, and duplicate keys are all preserved
  (§4). It is the cheap, lossless-round-trip type.
- **`jsonb` canonicalizes.** `'{ "a" : 1 }'::jsonb` parses to a node tree and stores a
  compact binary form (§2): insignificant whitespace dropped, object keys sorted, duplicate
  keys reduced last-wins, numbers held exactly as `decimal`. It is the type you query and
  index.
- **Numbers are exact.** A JSON number maps to jed `decimal` (PG `numeric`) — arbitrary
  precision, scale preserved (`1.50` keeps scale 2). **No binary float is ever introduced**
  on the JSON path (CLAUDE.md §8; the TS-core `f64` hazard avoided by construction).
- **Comparison.** `jsonb` has PostgreSQL's total btree order (§5): `= <> < <= > >=`,
  `ORDER BY`, `DISTINCT`, `GROUP BY` all work. `json` is **not comparable** (PG ships no
  operator class): those uses raise `42883`; only whole-value `IS [NOT] NULL` works.
- **Keys.** A `jsonb` column is **not** a valid `PRIMARY KEY` / index / `UNIQUE` target
  in the first slices (`0A000`, the array/text/decimal staged-key narrowing, §5); the
  order-preserving encoding is authored but unexercised ([encoding.md §2.13](encoding.md)).
  `json` is never keyable (it is not even comparable).
- **The dictionary door.** The `jsonb` value format reserves a per-token mechanism to encode
  a string or object key as a reference into a future column/table **string dictionary**
  (§3), without building the dictionary now. This is a pure reservation — zero bytes today.

---

## 2. The `jsonb` binary value format

`jsonb` is a self-describing **tagged-node tree** serialized depth-first, *not* a copy of
PostgreSQL's JEntry/offset-array layout. The design follows jed's container precedent
(array/composite — [array.md §4](array.md)): every node leads with a one-byte tag, and a
container concatenates its children's **bodies** with no per-child offset table (a reader is
self-delimiting by walking children in order). The whole document is one variable-length
**body** sitting behind the existing value-codec presence tag, so a large `jsonb` value
rides the [large-values.md](large-values.md) spill/compress disposition unchanged (it is
just another variable-length body, externalized/compressed largest-first like `text`/`bytea`).

### 2.1 Node tag byte

The first byte of every node is a tag: **low nibble = node kind**, **high nibble = flags**
(reserved `0` today, claimed by the dictionary door §3 later).

| tag (low nibble) | kind | payload |
|---|---|---|
| `0x0` `NTAG_NULL` | JSON `null` | (none) |
| `0x1` `NTAG_FALSE` | `false` | (none) |
| `0x2` `NTAG_TRUE` | `true` | (none) |
| `0x3` `NTAG_NUMBER` | number | the **decimal value body**, reused verbatim from the value codec ([format.md](../fileformat/format.md), `decimal`) — there is exactly one numeric codec in the engine |
| `0x4` `NTAG_STRING` | string (inline) | `len varint ‖ <len> UTF-8 bytes` (collation-`C` byte order) |
| `0x5` `NTAG_STRING_DICT` | string (dict-ref) | `dict_id varint` — **reserved**; an out-of-dictionary-slice reader rejects it as `data_corrupted` (`XX001`) (§3) |
| `0x6` `NTAG_ARRAY` | array | `count varint ‖ <count> child node bodies` in element order |
| `0x7` `NTAG_OBJECT` | object | `count varint ‖ <count> members`; each member = `key ‖ value`, where `key` is a string node (`0x4`/`0x5`) and `value` is any node, in canonical key order (§2.3) |

`varint` is the engine's existing unsigned varint (the same codec already used for counts/
lengths in container bodies). Booleans and `null` are a single byte total. JSON `null`
(`NTAG_NULL`) is a **node** inside the document and is wholly distinct from a SQL NULL `jsonb`
value (the lone presence-tag `0x01`, no body).

### 2.2 Worked examples (body bytes, after the present `0x00` presence tag)

| value | body |
|---|---|
| `null` | `00` |
| `true` | `02` |
| `123` | `03` ‖ dec(123) |
| `"hi"` | `04 02 68 69` |
| `[1,"a"]` | `06 02` ‖ `03`·dec(1) ‖ `04 01 61` |
| `{"b":1,"aa":2}` | `07 02` ‖ [`04 01 62` ‖ `03`·dec(1)] ‖ [`04 02 61 61` ‖ `03`·dec(2)] — **"b" precedes "aa"** (length 1 < length 2, §2.3) |
| SQL NULL `jsonb` | (no body — value is the lone `0x01` presence tag) |

### 2.3 Object key order and duplicate keys (the canonicalizer)

`jsonb` stores object members in a **canonical order: shorter key first, ties broken
bytewise** (length-then-bytewise — PostgreSQL's jsonb key order). Two reasons, both
load-bearing:

1. **It matches PG observably.** The order is visible through `jsonb_out`,
   `jsonb_object_keys`, `jsonb_each`, and `@>`/`?`. PG's canonical order *is* length-then-
   bytewise; choosing pure bytewise would force a documented divergence on every one of those.
2. **It kills the §8 iteration-order leak.** Keys are sorted at parse/canonicalization time,
   so the stored bytes are a pure function of the *value*, never of hashmap insertion or
   iteration order. The sort *is* the canonicalizer.

**Duplicate keys: last value wins.** When `jsonb_in` sees the same key twice in one object,
it keeps the **last** occurrence's value and drops the earlier (PG `jsonb` semantics).
De-duplication happens **before** sorting, so the stored object has unique keys in canonical
order. (`json` keeps duplicates verbatim — §4 — the deliberate observable difference between
the two types.) Both rules are deterministic: the stored bytes are a pure function of the
input text.

> **Divergence vs PG, ledgered:** none on the bytes-observable surface — the key order,
> dedup rule, and number representation all match PG. The one representational difference
> from PG is *internal* (jed's tagged-node layout ≠ PG's JEntry layout); it is invisible
> because jed owns its on-disk format (CLAUDE.md §1, "we own our surface").

---

## 3. The dictionary door (seam reserved, builder deferred)

The user requirement: leave the door open for a **column-level key (or general string)
dictionary** for `jsonb`, so a repeated key/string can be stored as a small reference
instead of inline bytes — designed now, **built later**. The reservation has three parts and
costs **zero bytes today** (the large-values precedent of reserving the `0x03`/`0x04`
compression forms ahead of the slice that emits them — [large-values.md](large-values.md)).

### 3.1 The inline-vs-reference mechanism (node tags, §2.1)

A string — whether an object **key** or a string **value** — is one node, encoded as either:

- **`NTAG_STRING` (`0x4`)** — inline: `len varint ‖ bytes`; or
- **`NTAG_STRING_DICT` (`0x5`)** — a reference: `dict_id varint` into the column/table
  dictionary.

A reader dispatches on the tag; both forms materialize to the identical
`JsonNode::String`. Because an object member stores its **key as a full string node**
(§2.1), the dictionary covers **keys and string values uniformly** — exactly the user's
"key (or even general string) dictionary." The high-nibble flag bits on every node tag are
reserved `= 0` for future sub-variants (e.g. a per-document local dictionary vs a column
dictionary) without a further format bump.

**Today's behavior (every pre-dictionary slice):** writers emit **only `NTAG_STRING`**;
a reader that encounters `NTAG_STRING_DICT` or any nonzero flag nibble raises
`data_corrupted` (`XX001`). The door is therefore a pure reservation — present in the format
spec, never in emitted bytes — so every file written before the dictionary slice is
byte-identical regardless of whether a later reader supports the dictionary.

### 3.2 Where the dictionary lives (catalog hook)

Reserve a `has_jsonb_dict` flag bit on the `jsonb` column entry, mirroring the v17
`has_collation` text-column flag ([format.md](../fileformat/format.md)) that writes nothing
when clear. When the bit is clear (every pre-dictionary slice), the column entry is
byte-identical to a plain scalar with no extra descriptor. When the dictionary slice lands
and the bit is set, the entry gains a **dictionary descriptor** — a `dict_root_page u32`
(the dictionary is itself a small B-tree / overflow structure allocated from the free-list,
mapping `dict_id → string`) and a `dict_count varint`. The node body only ever stores an
**opaque `dict_id`**; *where* that id resolves — a per-column dictionary, or a per-table
shared one keyed by a catalog-level entry-kind — is a storage-layer decision deferred to the
builder. Both scopes stay reachable because the node format does not encode the choice.

### 3.3 Determinism rules (pinned now so the door cannot diverge later)

A dictionary is shared state, so it must be deterministic or it breaks cross-core byte-
identity (§8). The rules are pinned now, the builder deferred:

1. **Id assignment is a pure function of the string set.** Dictionary ids are assigned by
   **sorting the candidate strings length-then-bytewise** (the §2.3 order) and numbering them
   `0,1,2,…`. So a string's id depends only on *which strings are in the dictionary*,
   identical across cores — never on insertion or scan order.
2. **Membership is a spec-pinned, data-shaped threshold.** *Which* strings enter the
   dictionary follows a deterministic, documented policy (e.g. "keys/strings occurring
   ≥ N times over a deterministic full scan; N is a spec constant"), authored as data — the
   same discipline as the large-values disposition constants — never a per-core heuristic.
3. **Encode choice is a pure function of membership.** A string node is `NTAG_STRING_DICT`
   iff the string is in the dictionary (with, if wanted, a pinned "store-smaller" tiebreak
   matching large-values' rule — adopt the ref iff its encoded size < the inline size).
4. **Goldens.** When the slice lands, ship `(input → bytes)` fixtures for both a
   dictionary-built and a non-dictionary column, byte-pinned `rust == go == ts == ruby`.

Net: the format reserves two node tags + the flag nibble, the catalog reserves a flag bit +
descriptor slot, and the determinism rules are fixed — and **none** of the builder/maintenance
ships. The dictionary lands additively as its own deferred slice (§12) with no format bump.

---

## 4. `json` (textual) storage

`json_in` **validates** that the input is well-formed JSON (RFC 8259) and stores the
**original UTF-8 bytes verbatim** — preserving insignificant whitespace, original key order,
and **duplicate keys** (PG `json` semantics, the deliberate contrast with `jsonb`). The
stored body is shaped exactly like a `text` body (presence tag + length-prefixed UTF-8) and
reuses the **same** large-value overflow + LZ4 disposition ([large-values.md](large-values.md)).
`json_out` returns the stored bytes verbatim (the identity on the stored text).

This makes `json` strictly cheaper than `jsonb` to add and to evaluate: there is no
canonicalization, so no canonicalization-order question arises and the stored bytes are
trivially a pure function of the input. The cost is that two textually-different-but-
semantically-equal `json` values store different bytes (`'{"a":1}'` ≠ `'{ "a":1 }'`) — which
is correct and PG-faithful.

A `json` value reaches the structural node tree (§1) only when an operator needs structure
(e.g. `->`): it is parsed on demand and dispatched to the same kernel as `jsonb`, except
where PG observably differs (`json_object_keys` preserves duplicate keys; `json_each`
preserves input order — recorded as data in the catalog, not branchy code; see
[json-sql-functions.md](json-sql-functions.md)).

### 4.1 Storage assignment

Every scalar write path uses the same storage-assignability rule: `INSERT ... SELECT`, constant
and expression defaults, ordinary `UPDATE`, and `ON CONFLICT DO UPDATE`. `json → json` and
`jsonb → jsonb` are identities, so textual `json` keeps its verbatim bytes and `jsonb` keeps its
canonical node tree. `jsonb → json` is the §6.1 **assignment cast**: the storage boundary renders
the node through `jsonb_out` and stores that canonical text as the `json` value. SQL `NULL` assigns
to either nullable type normally.

The reverse `json → jsonb` direction remains **explicit-only** (`value::jsonb`): jed does not
silently discard whitespace, key order, or duplicate keys on assignment. PostgreSQL admits that
direction as an assignment cast; rejecting it with `42804` is jed's deliberate strict-matrix
divergence. A bare string literal still adapts through `json_in` / `jsonb_in`, as described in §6.1.
Composite-column UPDATE, `jsonpath` columns, and the separately documented container-parameter /
upsert limitations are unaffected by this scalar rule.

---

## 5. Comparison / ordering

- **`json` is NOT comparable.** PostgreSQL ships no btree/hash operator class for `json`;
  jed matches it. `=` / `<>` / `<` / `ORDER BY` / `DISTINCT` / `GROUP BY` on `json` raise
  **`42883`** (no operator) at resolve time; a `json` `PRIMARY KEY` / index / `UNIQUE`
  raises **`0A000`** (no key encoding). Only whole-value `IS [NOT] NULL` works. `json` joins
  `float` as a non-comparable / non-keyable scalar — recorded in its `scalars.toml` row.

- **`jsonb` IS comparable** — PostgreSQL's total btree order, implemented as a hand-written
  **recursive comparator** in each core's value module (`eq3`/`lt3`/`gt3`), like
  array/range/composite (the catalog cannot express "recurse over a JSON tree"; CLAUDE.md §5
  forbids codegenning it). It yields a **definite boolean** — btree semantics like array/range,
  **not** composite's 3VL — because there are no SQL NULLs *inside* a document (JSON `null`
  is a concrete `NTAG_NULL` node, not SQL NULL). The order (PG's):

  1. **Type rank** (the outermost discriminator):
     `Object > Array > Boolean > Number > String > Null`.
  2. Within the same type:
     - **Null:** all equal.
     - **Boolean:** `false < true`.
     - **Number:** by `decimal` numeric value (scale-independent — `1.0` compares equal to
       `1.00`, the existing decimal rule).
     - **String:** by collation-`C` UTF-8 byte order (the existing `text` comparison).
     - **Array:** by **element count first** (fewer elements sort first — PG compares array
       length before contents), then element-wise recursively; the first differing element
       decides.
     - **Object:** by **member count first** (fewer members first — PG), then by comparing
       members pairwise in **stored canonical key order** (§2.3): compare keys
       (length-then-bytewise), then values recursively; the first differing key-or-value
       decides.

  A single comparator drives both `<` and `ORDER BY`, so they agree by construction (the
  §8 consistency requirement; jed sidesteps PG's own abbreviated-key inconsistency by having
  one comparator).

- **`jsonb`-as-key deferred `0A000` initially.** Comparison/ordering/grouping ship via the
  in-memory comparator — **no key bytes needed** (the array precedent: values are fully
  orderable without an order-preserving key encoding). A `jsonb` `PRIMARY KEY` / index /
  `UNIQUE` raises `0A000` in the first slices. The order-preserving encoding is **authored**
  in [encoding.md §2.13](encoding.md) (a type-rank discriminator byte, then per-kind
  self-delimiting bodies, recursively — mirroring the §2.11 range-key recursion) and marked
  unexercised; a follow-on slice (§12) exercises it. This is the established staged-key
  narrowing (text/decimal/bytea/array all carried it), recorded as a deliberate, time-boxed
  divergence, not a permanent one.

---

## 6. Casts & text I/O

### 6.1 Cast matrix rows ([casts.toml](../types/casts.toml))

| from | to | mode | behavior |
|---|---|---|---|
| `json` | `jsonb` | explicit | re-parse + canonicalize (sort keys, dedup last-wins, numbers→decimal) |
| `jsonb` | `json` | explicit / assignment | render canonical text via `jsonb_out` |
| `json` | `text` | explicit | identity on stored bytes (`json_out`) |
| `jsonb` | `text` | explicit | `jsonb_out` canonical form |
| `text` | `json` | explicit | runtime `json_in` (validate, store verbatim) |
| `text` | `jsonb` | explicit | runtime `jsonb_in` (parse + canonicalize) |

The `jsonb → json` assignment mode applies at every scalar storage boundary (§4.1). The reverse
row stays explicit-only even though PostgreSQL also admits it in assignment context; this is the
strict-matrix divergence recorded in §4.1 and the conformance override ledger.

**Literal vs runtime split** (the established jed convention — [casts.toml](../types/casts.toml)
comment block; [array.md §7](array.md)): a string-**literal** coerced by a named JSON type
(`'{"a":1}'::jsonb`, `jsonb '{"a":1}'`) routes through `jsonb_in`/`json_in` at *resolve*
time and is **not** a matrix row. The matrix rows above are the **runtime** casts on a
non-literal text expression; since the parser already exists, jed admits them (PG admits
them too).

### 6.2 Text I/O

- **`json_in`** — validate RFC-8259 well-formedness; store verbatim UTF-8 (§4). Malformed
  input → `22P02`.
- **`json_out`** — return the stored bytes verbatim.
- **`jsonb_in`** — parse to the node tree (numbers→`decimal`, keys deduped last-wins then
  sorted length-then-bytewise §2.3), encode the §2 body. Malformed input → `22P02`.
- **`jsonb_out`** — render the **canonical** PG style: one space after each `:` and `,`
  (`{"a": 1, "b": [1, 2]}`), keys in canonical order, numbers via the `decimal` renderer
  (scale preserved, e.g. `1.50`), strings JSON-escaped, `true`/`false`/`null` lowercase.
  Uses the existing **`T` render tag** (a printable-ASCII string, like bytea/uuid/array —
  [conformance.md §1](conformance.md)); **no new render tag**. Byte-identical to the PG
  oracle, verified by `rake corpus:check`.

### 6.3 Error surface

| failure | code |
|---|---|
| malformed JSON in `json_in` / `jsonb_in` (incl. literal cast) | `22P02` invalid_text_representation |
| `json`/`jsonb` value too large to store after compress + spill | `0A000` (the existing oversized narrowing) |
| `=` / `<` / `ORDER BY` / `DISTINCT` on `json` (non-comparable) | `42883` undefined_function |
| `json`/`jsonb` `PRIMARY KEY` / index / `UNIQUE` (`json` always; `jsonb` until the key slice) | `0A000` feature_not_supported |
| corrupt jsonb body (bad node tag, or reserved `0x5`/flag bits before the dictionary slice) | `XX001` data_corrupted |
| (reserved for the SQL/JSON **path** surface — [jsonpath.md](jsonpath.md)) invalid JSON text | `22032` invalid_json_text |

`22P02`, `42883`, `0A000`, `XX001` are already registered. `22032` (and the rest of the
`2203x` SQL/JSON class) is registered by the path surface — see
[jsonpath.md §error-codes](jsonpath.md) and [json-sql-functions.md](json-sql-functions.md).

---

## 7. `scalars.toml` rows (the type definitions the slices commit)

The three rows the J-/P-series slices add to [../types/scalars.toml](../types/scalars.toml)
(`jsonpath` lives here too, detailed in [jsonpath.md](jsonpath.md)):

| id | family | storable | comparable | keyable | stable type_code | notes |
|---|---|---|---|---|---|---|
| `json` | `json` | true | **false** | false | **18** | verbatim text body; not in `compare.toml` |
| `jsonb` | `jsonb` | true | **true** (self only) | deferred `0A000` | **19** | tagged-node body §2; recursive `eq3`/`lt3`/`gt3` |
| `jsonpath` | `jsonpath` | true | false | false | **20** | compiled-program type — [jsonpath.md](jsonpath.md) |

`json`/`jsonb`/`jsonpath` take no `bits`/`min`/`max`/`overflow` (those are integer-only
fields). `json` and `jsonpath` carry **no `encoding`** (non-keyable, like `float`); `jsonb`'s
`encoding` is authored as the deferred `jsonb` key (`§2.13`) and marked unexercised. The
`compare.toml` self-comparability entry exists for `jsonb` only; `json` is omitted (a value
that has no comparison family — the resolver maps any comparison attempt to `42883`).

---

## 8. Determinism

Everything on the JSON path is deterministic and cross-core byte-identical:

- **Numbers are `decimal`** — exact, no `f64`, no float formatting in the compare or text
  path (CLAUDE.md §8; [determinism.md](determinism.md)).
- **`jsonb` canonical key order** (length-then-bytewise) makes stored bytes and rendered
  text a pure function of the value — no hashmap-iteration-order leak (§2.3).
- **`json` verbatim storage** makes its bytes a pure function of the input text (§4).
- **Duplicate-key dedup** (`jsonb`, last-wins) and the recursive comparator (§5) are pure
  functions of inputs.
- **The dictionary door** (§3.3) pins deterministic id assignment + membership *before* the
  builder ships, so opening the door cannot introduce divergence.

No part of the JSON surface reads the wall clock or entropy **except** the SQL/JSON path
`.datetime()` method and the `_tz` path-query variants, which route through the existing
host clock/tz seam ([entropy.md](entropy.md)) and are therefore deterministic-given-the-seam
(detailed in [jsonpath.md](jsonpath.md)).

---

## 9. Conformance & capabilities

New capability ids (added to [../conformance/manifest.toml](../conformance/manifest.toml)
with their slice) and the suite they gate:

| capability | gates |
|---|---|
| `types.jsonb` / `types.json` | declare/store/round-trip a `jsonb` / `json` column |
| `types.jsonb_compare` | `jsonb` `=`/`<`/`ORDER BY`/`DISTINCT`/`GROUP BY` |
| `types.json_casts` | the §6.1 cast rows |
| `func.jsonb_access` | `->` `->>` `#>` `#>>` |
| `func.jsonb_contains` | `@>` `<@` `?` `?|` `?&` |
| `func.jsonb_mutate` | `||` `-` `#-` |

A new `json` **profile** bundles these plus the path/function/table capabilities defined in
the sibling docs. Tests live in a new `spec/conformance/suites/json/` suite, oracle-checked
against live PG (`rake corpus:check`), with any deliberate divergence recorded in the
override ledger. Per-core unit tests cover only what the corpus cannot express (CLAUDE.md
§10): the on-disk golden round-trip, the `42883`/`0A000` divergences, and catalog
introspection.

---

## 10. Operators (summary; full catalog entries in the sibling docs)

The classic `jsonb` operator surface, sliced in J4–J6 (full `catalog.toml` entries +
kernels in [json-sql-functions.md](json-sql-functions.md)):

| op | meaning | result | slice |
|---|---|---|---|
| `->` | get field (by key) / element (by int index) | `json`/`jsonb` | J4 |
| `->>` | get field/element as text | `text` | J4 |
| `#>` | get at path (`text[]`) | `json`/`jsonb` | J4 |
| `#>>` | get at path as text | `text` | J4 |
| `@>` / `<@` | contains / contained-by | `boolean` | J5 |
| `?` / `?|` / `?&` | key exists / any / all | `boolean` | J5 |
| `||` | concatenate / merge | `jsonb` | J6 |
| `-` | delete key / element / each-of-`text[]` | `jsonb` | J6 |
| `#-` | delete at path (`text[]`) | `jsonb` | J6 |
| `@?` / `@@` | jsonpath exists / match | `boolean` | P2 ([jsonpath.md](jsonpath.md)) |

`@>`/`<@`/`?`/`?|`/`?&` are exactly the predicates a future GIN `jsonb_ops` opclass
accelerates (the [gin.md](gin.md) seam already seats it — TODO.md line 388); J5 ships them as
sequential predicates, GIN pushdown a deferred follow-on (§12), the range-GiST precedent.

---

## 11. On-disk format & version

[format.md](../fileformat/format.md) records: stable type codes **18 (`json`)**,
**19 (`jsonb`)**, **20 (`jsonpath`)**; the `jsonb` node-body layout (§2.1); the json
verbatim-text body (§4); and the reserved `has_jsonb_dict` column-entry flag (§3.2). The
`format_version` bumps **once**, at the first storable-JSON slice (the current tip is `v18`
→ `v19`); later JSON slices that add only operators/functions are format-orthogonal. A
`jsonb` column needs no extra catalog descriptor bytes (just `type_code 19`) — like
`text`/`bytea`/`uuid`, unlike array/range/composite — until the dictionary slice sets the
reserved flag.

---

## 12. Delivery — vertical slices

Each slice is independently shippable (`rake ci` green) with its own capability + oracle-
checked conformance entries, mirroring the composite/array/range cadence. Full sequencing
across all four JSON docs is in [TODO.md]. Critical path:
**J0 → {J1, J2, J3} → C0 → P1 → {P2, S2} → {R1, T1}**.

- **J0** — `json`/`jsonb` scalar arms + `json_in`/`json_out`/`jsonb_in`/`jsonb_out` + the
  `'…'::jsonb` literal cast. No columns yet (declaring one is `0A000` until J1, the array
  S0→S2 staging). Reserves the dictionary door (zero bytes). Capability `types.jsonb_literal`.
- **J1 / J1b** — storable `jsonb` column + the §2 body codec + the `format_version` bump +
  spill/compress + golden `jsonb_table.jed`; then storable `json` (verbatim body) + golden
  `json_table.jed`. Capabilities `types.jsonb` / `types.json`.
- **J2** — `jsonb` comparison/ordering (§5); `json` non-comparable (`42883`).
- **J3** — casts (§6).
- **J4 / J5 / J6** — accessor / containment / mutation operators (§10).
- **C0** — the shared FROM-clause column-definition-list facility ([json-table.md §1](json-table.md)),
  the keystone for record functions + `JSON_TABLE`.
- **P/S/B/R/T series** — the path language, SQL/JSON standard functions, builders/SRFs/
  aggregates, record-returning functions, and `JSON_TABLE` — in [jsonpath.md](jsonpath.md),
  [json-sql-functions.md](json-sql-functions.md), [json-table.md](json-table.md).

**Explicitly deferred `0A000` follow-ons:** the key/string **dictionary builder** (opens the
§3 door — its own slice, its own goldens); **`jsonb` as PK/index** (exercise
[encoding.md §2.13](encoding.md)); the **GIN `jsonb_ops`** opclass for `@>`/`?` ([gin.md](gin.md)).
