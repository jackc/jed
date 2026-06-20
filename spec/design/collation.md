# Collation — design

> Linguistic (locale-aware) collation for `text`: dictionary-style ordering (`ä` near `a`,
> not after `z`) layered on the existing UTF-8 `text` type via a `COLLATE` clause, a per-column
> collation, and a **per-database default collation** stored in the file. The engine **owns the
> collation algorithm** (a hand-written Unicode Collation Algorithm, UTS #10, in every core)
> and does **not vendor collation tables in the binary**; instead a collation is **explicitly
> loaded** from the host environment (`db.LoadHostCollation("en-US")`) or from an explicit
> definition, **compiled into jed's own table**, and — by default — **baked into the database
> file** so a collated index can never drift or corrupt across machines or jed versions. A
> collation is a **first-class, portable artifact**: extract it once where ICU/CLDR exists,
> **save it to a buffer/file/stream**, ship it anywhere, and import it into a database — each
> carrying an optional human-readable **provenance description** (e.g. `Go 1.26.3 / Linux 7.1 /
> ICU 73`). A small-footprint **reference mode** (store only the collation's name + hash) is an
> explicit opt-out for controlled environments. This doc is the contract all three cores implement in
> lockstep (CLAUDE.md §2). The `text` type and the `C`-collation baseline are in
> [types.md §11](types.md); the key-encoding rule in [encoding.md §2.4](encoding.md); the
> catalog/byte layout in [../fileformat/format.md](../fileformat/format.md); the LZ4 codec that
> compresses a baked table in [large-values.md](large-values.md); the host-seam pattern in
> [hosts.md](hosts.md) and [entropy.md](entropy.md); the determinism stance in
> [determinism.md §3](determinism.md); the cost contract in [cost.md](cost.md); the
> data-over-code framing in [extensibility.md §4.1](extensibility.md).
>
> **Status: design ratified; slice 1a (the byte-format foundation) authored; no core code yet.**
> No core ships a non-`C` collation today; `text` is `C` (byte / code-point order) and needs
> **zero data** ([types.md §11](types.md)). **Slice 1a** has landed the byte-format foundation in
> [../collation/README.md](../collation/README.md) (the definition format, the compiled-table
> layout, the `.coll` artifact, the sort key) plus the dev fixtures — authored ahead of code
> exactly as the text/decimal key-encoding rules were ([encoding.md §2.4](encoding.md) "authored,
> not yet exercised"). The implementation slices (§14) land against it. Two decisions are
> **confirmed**: the definition format is the **UCA/CLDR standards** (DUCET `allkeys.txt` + LDML),
> and the cores build **`CompileCollation` in-core first** (no external authoring tool). This doc
> pins no `format_version` or `type_code` number: the version bump and the new catalog
> `entry_kind` are claimed at slice 1d, against whatever the master tip is then.

Collation is the rule for **ordering and equating** text, layered on the *encoding* (which
maps characters to bytes — jed commits to UTF-8 everywhere). jed ships exactly one collation
today, **`C`** (compare raw UTF-8 bytes by `memcmp`, which for UTF-8 equals Unicode code-point
order). `C` is table-free, fixed, built in, and identical on every platform/core/version
forever — which is *why* it is the right baseline for a no-reference-implementation, byte-exact,
multi-core engine ([types.md §11](types.md), CLAUDE.md §2/§8). Its price is that it is not
"human": `'B' < 'a'`, digits before letters, accented characters after all ASCII. Linguistic
collation fixes that — at the cost of data tables and a versioned algorithm, the two things
this document makes safe.

## 1. Surface and lifecycle

A collation must be **loaded into the database before it can be used.** This is a deliberate,
explicit lifecycle (replacing an earlier "auto-snapshot a collation the first time an index
uses it" idea, now reverted — §14): loading is the one place the host environment is consulted,
and making it explicit keeps that boundary visible and keeps every later *use* pure (§11).

```
// host API (privileged — not untrusted SQL, §11). A `Collation` is a DB-independent value (§4).

// produce a Collation value:
coll := ExtractHostCollation("en-US")         // from the host environment (auto-fills provenance, §4)
coll := CompileCollation("en-US", defReader)  // from an explicit canonical definition (deterministic)
coll := OpenCollation(reader)                  // from a previously-saved jed artifact (buffer/file/stream)

// serialize a Collation value back out (provenance + table travel with it):
SaveCollation(coll, writer)                    // → a jed artifact (buffer/file/stream)

// move a Collation in/out of a database:
db.ImportCollation(coll)                        // bake into the file (default); .Reference ⇒ name+hash only
db.ExportCollation("en-US")                     // pull a baked collation back out as a Collation value
db.SetDefaultCollation("en-US")                 // set the per-database default (must be imported)

// convenience — the common case is "extract + import, baked":
db.LoadHostCollation("en-US")                   // == db.ImportCollation(ExtractHostCollation("en-US"))
```

```sql
-- SQL surface, once a collation is loaded:
CREATE TABLE people (id i32 PRIMARY KEY, name text COLLATE "en-US")
CREATE INDEX ON people (name)                   -- ordered by the column's en-US collation
SELECT name FROM people ORDER BY name             -- en-US order
SELECT name FROM people ORDER BY name COLLATE "C" -- override: byte order
SELECT 'ä' < 'z' COLLATE "de"                      -- per-expression collation (de must be loaded)
```

- **`COLLATE "name"`** is a postfix operator on a text expression yielding the same value with
  a different collation for the surrounding comparison/sort. It binds tighter than the
  comparison operators, looser than `||`/subscript — PG's precedence ([grammar.md](grammar.md)
  when it lands). Naming a collation that has not been loaded is **`42704`**
  (`undefined_object`), the same code arrays/ranges raise for an unknown element type.
- **Collation names are quoted identifiers** (they contain hyphens): `"C"`, `"en-US"`, `"de"`,
  `"sv"`. `"C"` is always available; every other name must be loaded first.
- **Per-database default collation** (§3). Every database has a default collation **stored in
  its file**; an un-annotated `text` column uses it. It is **`C` at creation** and can be set
  to any *loaded* collation via `db.SetDefaultCollation`. This is the answer to "don't hard-code
  `C`, and don't depend on the host `LC_COLLATE`": the default is a deliberate, persisted,
  per-database choice — not an ambient host locale, not a wired-in constant.
- **Per-column collation.** A `text` column may carry an explicit collation
  (`name text COLLATE "en-US"`); absent a clause it inherits the **database default**. The
  column collation is the default for every comparison / `ORDER BY` / `DISTINCT` / `GROUP BY` /
  `PRIMARY KEY` / `UNIQUE` / index over that column; an explicit query `COLLATE` overrides it
  for that expression.
- **Collation derivation in expressions** follows PG's rules: an explicit `COLLATE` is
  *explicit*; a column reference is *implicit*; combining two different implicit collations in
  one operator is a **conflict** → `42P22` (`indeterminate_collation`), resolved by an explicit
  `COLLATE`. A literal has no collation and takes its neighbour's.
- **Provenance + introspection.** Each loaded collation carries an optional, human-readable
  **description** recording where it came from — auto-filled by `ExtractHostCollation` with the
  core/OS/library identity (e.g. `Go 1.26.3 / Linux 7.1 / ICU 73`), settable on any `Collation`,
  preserved through save/open/import, and surfaced by introspection (`db.Collations()` →
  name, version, hash, mode, description). It is descriptive metadata only — **excluded from the
  content hash** (§4), so it never affects ordering, dedup, or the reference-mode hash check.

## 2. The fixed architecture: jed owns the algorithm; tables are generated, not vendored

Two options are **ruled out before any design choice:**

- **Delegating ordering to the host's ICU/glibc *at query time* is impossible here** — not
  merely because an OS upgrade reorders strings (PostgreSQL's silent-index-corruption trap),
  but because Rust's linked ICU, Go's `x/text/collate`, and TS's `Intl.Collator` produce
  *different orderings from each other on day one*. Query-time host delegation breaks
  **cross-core byte-identity** (CLAUDE.md §8) immediately ([types.md §11](types.md),
  [determinism.md §3](determinism.md)).
- **Letting collation be a sanctioned query-time non-determinism** (a ledger exception) is
  refused: [determinism.md §3](determinism.md) requires linguistic collation to be turned
  "back into deterministic data — never a sanctioned exception."

But jed **also does not vendor the collation tables in its binary** — a deliberate change from a
"version-pin the DUCET + a curated locale set into every core" model, which would bloat every
core with megabytes of data and force a fixed, hard-to-grow locale set. Instead the
architecture has three layers, and the cross-core contract sits on the lower two:

1. **The jed collation table** — jed's own compiled, in-file, executable form (collation
   elements + multi-level weights, §6). What the executor runs on.
2. **The executor** (table → ordering / sort key, §6) and **the compiler** (a canonical
   collation *definition* → a jed table, §6) — both **jed-owned, hand-written per core, spec'd**
   (CLAUDE.md §5 forbids codegenning them), and **cross-core byte-identical given identical
   input**. These two are the cross-core contract, verified by byte fixtures (§10), exactly the
   composite/array precedent ([extensibility.md §4.1](extensibility.md)).
3. **The host seam** (`ExtractHostCollation`, §4) — the **only** producer that touches the host
   environment, and the **only** layer that is *not* part of the cross-core contract. Its output
   (a jed table, optionally saved as an artifact, §4) is captured into the file and is
   authoritative thereafter. (The other producers — `CompileCollation` from a definition,
   `OpenCollation` from a saved artifact — are deterministic and host-free.)

> **The determinism boundary, stated once:** cross-core byte-identity is a property of *a jed
> table + the executor*, read back from a file. The import is a host boundary; its variation is
> **contained by baking** (the table is frozen into the file, §3) and by a **content hash**
> (§3). A query never observes the import — it runs over the baked table — so collation *use*
> stays fully inside the deterministic contract while collation *loading* is free to read the
> messy host environment. This is the same shape as the storage seam (the host supplies bytes;
> the engine's behavior over those bytes is fixed), not the clock seam (a per-query draw).

## 3. Where the data lives: load explicitly, bake by default

Once a collation is imported into a jed table (§2 layer 3 → 1), the table needs a home. Two
modes, chosen per loaded collation:

- **Baked (the default).** The full jed table is stored **in the database file** (§5),
  LZ4-compressed ([large-values.md](large-values.md)). The file is **self-contained and
  portable**: it opens and orders identically on any machine, any jed version, with no host
  dependency — exactly the property a `rust`-written file already has for `go`/`ts`/`ruby`
  (CLAUDE.md §8) and that composite/array values already have (their types live in the file).
  For a text index this is decisive: **the collation order *is* the index order**, so freezing
  the weights next to the B-tree they order is the only thing that *eliminates* the
  glibc-2.28 corruption class rather than relocating it from "OS upgrade" to "jed upgrade." A
  baked collated index can never drift.
- **Reference (opt-in, name + hash only).** The file stores just the collation's **`(name,
  source-version, content-hash)`**, not the table. On open (or lazily on first use), jed
  **re-imports** the collation from the host and **verifies the hash**; a match uses it, a
  mismatch **hard-fails loudly** (a structured error, never silently-wrong rows — the
  `format_version` 7 checksum discipline, [storage.md §6](storage.md)). This trades the
  self-contained-file identity for ~zero footprint, and is intended for **small databases in
  known, controlled environments** where the host collation is guaranteed consistent. It pairs
  most safely with a collation produced deterministically (`CompileCollation` / a saved
  artifact, §4) or with identical host environments; the hash guards correctness regardless.

**Baked is the default and preserves the jed identity; reference is the deliberate
small-footprint opt-out.** A database that uses only `C` (the creation default) carries **zero**
collation data either way.

Every collated index also records the `(name, version, hash)` it was built under (the stamp).
Under baking this is normally a tautology — the table travels with the index — but it is what
makes a deliberate re-collation (§12) a *controlled* event and what detects on-disk tampering
of a baked table.

## 4. Producing, saving, and importing a collation

A **`Collation`** is a first-class, database-independent value: a jed table (§6) plus its
metadata (`name`, source `version`, content `hash`, optional `description`). The lifecycle is
**three composable stages** — produce a `Collation`, optionally save/ship it, import it into a
database — rather than one fused call. The split is what lets you extract a collation **once**
on a machine that has ICU/CLDR, save the artifact, and reuse it everywhere (including on cores
or machines that lack the host data), and it is what keeps the cross-core determinism boundary
crisp (§2): only the *extract* producer touches the host.

**Produce a `Collation`** (each returns a value; none writes to a database):

- **`ExtractHostCollation(name) -> Collation` — convenience, host-dependent.** The host author
  just names a collation; if the host has it (`en-US`), jed imports it with no further input. The
  extractor prefers to read the host's collation **data** directly (ICU's bundled data, system
  locale data) and normalize it into a jed table; where none is readable it **falls back to
  deriving the order from the host's runtime collator** (probing) — slower and only approximately
  faithful, an accepted last resort. It **auto-fills the `description`** with the core/OS/library
  identity. Because the result depends on the host's library/version, this producer is **not
  cross-core-deterministic** — which is exactly why its output is baked + hashed (§3) and why the
  corpus does not use it (§10).
- **`CompileCollation(name, definitionReader) -> Collation` — deterministic.** Compiles an
  explicit canonical **definition** (§9) — UCA root weights + LDML-style tailoring — into a jed
  table that is **byte-identical on every core**. The reproducible producer.
- **`OpenCollation(reader) -> Collation` — deterministic.** Deserializes a **previously-saved
  jed artifact** (from a byte buffer, file, or stream). No compilation, no host access — it just
  reads jed bytes, so it is the fastest and most portable producer and the one the corpus uses
  (§10).

**Save a `Collation`** (database-independent serialization):

- **`SaveCollation(coll, writer)` / `coll.Bytes()`** writes the jed **artifact** — a small
  self-describing container (magic + format version + `name` + `version` + `hash` +
  `description` + the compiled table, the table LZ4-compressed [large-values.md](large-values.md))
  — to a buffer, file, or stream. `OpenCollation` is its exact inverse; the round-trip is
  byte-identical on every core (§10). The artifact *is* the portable interchange form — the same
  bytes that get baked into a database file (§5) and the form the `spec/collation/` fixtures take
  (§9).

**Move a `Collation` in/out of a database:**

- **`db.ImportCollation(coll) -> name`** places the value into the database catalog — **baked**
  by default (the full table, §5) or **reference** (`coll` imported as name+hash only, §3).
- **`db.ExportCollation(name) -> Collation`** is the inverse: pull a baked collation back out as
  a `Collation` value (then `SaveCollation` it, or `ImportCollation` it into another database).
- **`db.LoadHostCollation(name)`** is the one-call convenience for the common case —
  `db.ImportCollation(ExtractHostCollation(name))`, baked.

Importing is **idempotent** by `(name, hash)`; importing a *different* table under a name already
in use by a persisted structure is an error (it would invalidate that structure — re-collation is
the explicit path, §12). The `description` is **not** part of the `hash` (§1), so re-importing or
reference-mode re-checking an otherwise-identical table with a different description still
matches.

**`C` is never produced or imported** — it is table-free and built in.

## 5. On-disk representation

Two additive changes to the file, both bumping `format_version` at the implementation slice:

- **The per-database default collation** (§1) is a small field in the database header / root
  catalog — a collation name (empty ⇒ `C`). It references a loaded collation snapshot when
  non-`C`.
- **A loaded collation is a new kind-tagged catalog entry.** The catalog already carries a
  leading `entry_kind` u8 per entry (`0` table, `1` composite type, `2` sequence —
  [format.md](../fileformat/format.md)); collation snapshots take the **next kind** (`3`), with
  emission order extended to *composite types → sequences → collation snapshots → tables* so a
  table/index entry that references a snapshot is read after it. A **collation-snapshot entry**
  holds:
  - the **name** (`"en-US"`),
  - the **`(unicode_version, cldr_version)`** (or an opaque source-version tag for a host
    import) and a **content hash** of the resolved table (the stamp, §3),
  - the optional **provenance description** (§1) — a length-prefixed UTF-8 string, **not**
    covered by the content hash,
  - a **storage-mode flag** — *baked* or *reference*,
  - for **baked**: the **compiled jed table**, LZ4-compressed ([large-values.md](large-values.md));
    for **reference**: nothing further (the name + version + hash + description above are the
    whole entry).

A baked snapshot is **the same bytes as a saved `Collation` artifact** (§4) wrapped in the
catalog framing — so `db.ExportCollation` is a near-copy and the on-disk goldens (§10) double as
artifact fixtures.

The per-column collation rides the slot [format.md](../fileformat/format.md) already reserves
for it (the per-column flags + typmod-adjacent field, where `varchar(n)` and the composite/array
type descriptors live). An **index entry** records the snapshot it was built under by
`(name, version, hash)`.

Because a baked snapshot is the same self-describing, LZ4-compressed, checksummed catalog data
every other catalog object already is, it inherits the §8 cross-core byte-identity for free: all
cores read identical baked bytes → compute identical sort keys (§8) → produce a byte-identical
collated B-tree that lands in the goldens (§10).

## 6. The algorithm: a compiler and an executor

Each core implements **two** hand-written collation routines (CLAUDE.md §5 forbids codegenning
either), both deterministic and cross-core byte-identical given identical input:

**The compiler — definition → jed table.** Input is a canonical collation *definition* (§9): the
UCA `allkeys.txt`-style root weights plus LDML-style tailoring rules (the diffs that move/merge
letters — `sv` sorts `å ä ö` after `z`; `de` phonebook folds `ä`→`ae`; Czech `ch` is a
contraction). Output is jed's compiled table (collation elements with multi-level weights,
contractions, expansions) — the table a `Collation` value (§4) wraps. This is what
`CompileCollation` runs; `ExtractHostCollation` either feeds the compiler a definition normalized
from host data or builds the table directly; `OpenCollation` skips the compiler entirely and
reads an already-compiled table from a saved artifact (§4).

**The executor — table → ordering.** The **Unicode Collation Algorithm (UTS #10)** over a jed
table:

1. **Collation elements.** Map the input's code points to collation elements via the table
   (root, as tailored).
2. **Multi-level weights / sort key.** Each element carries weights at levels: **L1 primary**
   (base letter — `a`=`A`=`á`), **L2 secondary** (accents — `a`<`á`), **L3 tertiary** (case —
   `a`<`A`), and a final **identical** level (code point, the `C` tie-break). Build the **sort
   key** by concatenating all L1 weights, a separator, all L2, a separator, all L3, a separator,
   then the identical level (the §2.4 C-key of the original string). Byte-exact in
   [../collation/README.md §4](../collation/README.md).
3. **Compare** by `memcmp` of sort keys — equal to the collation's logical order by
   construction. The sort key is the bridge to memcmp storage (§8).

**Deterministic vs nondeterministic collations** (PG's terms; *deterministic* here is a
*per-collation* property — whether collation-equality implies byte-equality — distinct from
jed's engine-wide cross-core determinism):

- A **deterministic collation** appends the **identical level**, so its order is **total** and
  **collation-equality coincides with byte-identity**: `x = y` iff same UTF-8 bytes (`'a' ≠
  'A'`, they merely sort adjacently). Every collation in the first slice is deterministic.
- A **nondeterministic collation** stops before the identical level, so `'café' = 'cafe'` and
  `'a' = 'A'` — distinct byte strings that are *equal*. This breaks the clean
  PK/UNIQUE/DISTINCT/hashing story (§7) and is **deferred** (§14).

**Variable weighting** (spaces/punctuation — UCA *non-ignorable* vs *shifted*) is fixed at
**non-ignorable** in the first slice (simplest, fully deterministic); CLDR/ICU's per-locale
*shifted* default is a deferred refinement (§14), pinned against the live `postgres:18` oracle.

## 7. Comparison, equality, and the relational operators

With only **deterministic** collations in the first slice (§6), the relational story is a pure
**re-ordering**, never a re-grouping:

- **Ordering** (`< <= > >= ORDER BY`) uses the collation's sort key; the order is **total**
  (identical-level tie-break), so `ORDER BY name` is fully deterministic including ties, and the
  final cross-column tie-break by primary key ([encoding.md](encoding.md), CLAUDE.md §8) is
  unchanged.
- **Equality, `DISTINCT`, `GROUP BY`, `UNIQUE`, `PRIMARY KEY`** are **unchanged from the `C`
  story**, because deterministic-collation equality *is* byte-identity (§6): `'a'`/`'A'` are two
  distinct values under any deterministic collation, so a `UNIQUE(name COLLATE "en-US")` admits
  both — identical grouping to `C`, only the scan order differs. This is what lets collation land
  as an *ordering feature only*, without touching uniqueness/hashing/DISTINCT.
- **Three-valued NULL logic** is unchanged; collation is a property of the non-NULL text
  comparison only.
- **`COLLATE` conflict** (`42P22`) and **unloaded collation** (`42704`) are the new errors in
  this path.
- **`LIKE` / pattern matching** under a non-`C` collation is **deferred** — the first slice
  evaluates `LIKE` and the pattern operators by **`C` (byte) semantics regardless of operand
  collation** (§14), matching the spirit of PG's restriction under nondeterministic collations.

## 8. Key encoding: sort keys keep `memcmp` storage intact

[encoding.md §1](encoding.md) commits the storage layer to **stored order == logical order by
`memcmp`, with no separate runtime comparator**. A collated index honors it via the **UCA sort
key** (§6): the key bytes are *not* the raw UTF-8 (that is the `C` special case,
[encoding.md §2.4](encoding.md)) but the sort key, whose `memcmp` order **is** the collation
order by construction.

The collated text key component (a new sub-section of [encoding.md §2](encoding.md), authored
when the slice lands, mirroring §2.4); the byte-exact layout is pinned in
[../collation/README.md §4](../collation/README.md):

```
L1-weights ‖ 0x0000 ‖ L2-weights ‖ 0x0000 ‖ L3-weights ‖ 0x0000 ‖ C-key(original UTF-8 via §2.4)
```

- The **level-separated sort key** orders the entry by the collation. Weights are `u16`
  big-endian and every emitted weight is `≥ 0x0001` (ignorable `0x0000` weights are skipped), so
  the two-byte `0x0000` level separator sorts **before** any weight — a level that is a prefix of
  another's sorts first ([../collation/README.md §4](../collation/README.md)).
- The appended **`C`-key of the original string** ([encoding.md §2.4](encoding.md)) does two
  jobs at once: it is the **identical-level tie-break** (totality, §6) *and* it makes the
  original **recoverable from the key** — required for a `PRIMARY KEY`, since a sort key alone is
  not reversible. (A *secondary* index can store `sortkey ‖ pk` instead and fetch the row via
  the PK.)
- **Descending / nullable** reuse the existing whole-component bitwise inversion and the
  nullable tag byte ([encoding.md §2.2/§2.3](encoding.md)) unchanged.

The trade is **key size** (a UCA sort key is ~2–3× the source, and the PK form also carries the
original) — the documented price of keeping one `memcmp` order rather than a runtime comparator.
The sort key is produced by the **baked** table (§5), so every core emits identical key bytes →
byte-identical collated B-trees.

## 9. The data: the host seam, the definition format, and the portable artifact

The binary ships **no collation data** beyond `C` — only the compiler and executor code (§6),
both small. Non-`C` collation data enters via the three producers (§4), in increasing order of
determinism and portability:

- **The host seam** (`ExtractHostCollation`) — host-specific access to whatever the platform
  provides (read ICU/system collation data; else probe the runtime collator). The per-core,
  host-dependent layer; **not** in the cross-core contract, tested per-core (§10).
- **The canonical definition format** (`CompileCollation`) — a well-defined source jed's compiler
  accepts: UCA root weights (`allkeys.txt` form) + LDML-style tailoring rules. An embedder or the
  test corpus supplies it; the compiler is cross-core-deterministic over it (§6).
- **The portable jed artifact** (`SaveCollation` / `OpenCollation`) — the **compiled** table
  plus its metadata in a self-describing container (§4). This is the canonical **interchange
  form**: extract or compile once, save the artifact, and every consumer thereafter just
  `OpenCollation`s identical jed bytes — no host, no recompilation. A baked catalog snapshot (§5)
  is the same bytes in catalog framing.

**`spec/collation/`** (a new spec data directory, parallel to `spec/encoding/`) holds the
**byte-format spec, fixtures, and verification vectors** — *repo data, not shipped in the
binary* — used by the corpus and goldens. The **byte formats are pinned in
[../collation/README.md](../collation/README.md)** (the definition format §1, the compiled table
§2, the `.coll` artifact §3, the sort key §4 — authored in slice 1a). The directory holds:

- the **definition format spec** (DUCET `allkeys.txt` subset + LDML tailoring subset) and the
  pinned `(unicode_version, cldr_version)` of the real root when it lands,
- a small **root definition fixture** + at least one **tailoring fixture** (the dev fixtures
  `dev-root.allkeys` + `dev-nordic.ldml` in 1a; the curated `en-US`, `de`, `fr`, `es`, `sv`, `da`
  set — the last two for the sharp `å ä ö`/`æ ø` after-`z` cases — as a follow-on),
- the **saved artifacts** those definitions compile to (`.coll` files) — what the corpus
  `OpenCollation`s for a deterministic, host-free load,
- **compiler vectors** — `(definition fixture) → (expected artifact / jed table bytes)`,
- **executor / sort-key vectors** — `(collation, string) → (sort-key bytes)`, the §8 byte-fixture
  pattern (CLAUDE.md §8) and the primary cross-core contract for the algorithm.

These exist so the corpus can load collations *deterministically* — `OpenCollation` (or
`CompileCollation`) from a fixture, never `ExtractHostCollation` — independent of any host.

## 10. Cross-core determinism and verification

Collation is a §8 divergence hotspot handled by the established machinery:

- **Compiler vectors + executor (sort-key) vectors** (§9) assert the two cross-core-contract
  routines (§2) directly — including the TS UTF-16-vs-code-point trap that already bites `C`
  ([types.md §11](types.md), the astral-character case).
- **Artifact round-trip** — `OpenCollation` then `SaveCollation` reproduces the input artifact
  **byte-for-byte on every core** (the `Collation` serialization is itself a §8 byte-identity
  contract, like the file format). Note the round-trip preserves the `description` *verbatim* —
  the description is only *generated* (and thus host/core-dependent) by `ExtractHostCollation`,
  never regenerated on open — so artifact identity holds for a given artifact on all cores.
- **A golden file containing a baked collation snapshot + a collated index** extends the
  byte-exact on-disk round-trip (`rust == go == ts == ruby`, CLAUDE.md §8) — pinning the baked
  table bytes and the collated B-tree's key bytes in one fixture (and, since a snapshot is an
  artifact in catalog framing (§5), doubling as an artifact golden).
- **Conformance entries** drive collation via `OpenCollation`/`CompileCollation` **from a pinned
  fixture** (never `ExtractHostCollation`), so all three cores read/compile the identical input →
  identical table → identical orderings; oracle-checked against `postgres:18` where jed matches PG
  and overridden-with-reason where it diverges (§15).
- **`ExtractHostCollation` (the host seam) is tested per core**, against that core's own host —
  the [conformance.md](conformance.md)/CLAUDE.md §10 carve-out for "what the corpus cannot
  express" (host introspection / platform-specific behavior), since the host path is
  *deliberately* not cross-core-identical (§2/§4).

## 11. Untrusted-query safety, cost, and the determinism ledger

- **Loading is a privileged host op; using is pure** (CLAUDE.md §13). The collation producers,
  serializers, and import/export are **host-API operations, never part of the untrusted-SQL
  surface** — an adversarial query cannot trigger a host read or load a table by naming a
  collation (it can only *use* already-imported ones, or get `42704`). Only
  **`ExtractHostCollation` actually reaches the host environment** (filesystem / collation
  library); `CompileCollation`/`OpenCollation`/`SaveCollation` are pure functions over
  caller-supplied bytes (no host reach), kept off the untrusted surface because they construct
  trusted catalog data, not because they touch the host. Once imported, *using* a collation is
  **pure** — a string and a baked table in, a sort key out; no host reach, no I/O, no
  nondeterminism.
- **Bounded cost.** Sort-key generation is metered by a `collate` cost unit per code point
  (table-bounded lookups, bounded contractions/expansions), so a collated `ORDER BY` over a
  large input is cost-ceilinged ([cost.md](cost.md)). The unit joins the shared cost schedule at
  the slice.
- **Collation *use* stays OUT of the determinism ledger.** Because a query runs over a baked
  table with a jed-owned executor, it is a deterministic function of its inputs — precisely the
  outcome [determinism.md §3](determinism.md) demands. Collation **loading** is a host
  configuration boundary (like *which file you opened*), not a query-time draw, so it needs no
  ledger entry either: no query observes the import (§2).

## 12. Migration and version adoption

The point of baking (§3) is that **a jed upgrade cannot break an existing baked file:**

- **Opening an old (baked) file with a new jed just works** — collated structures read against
  the *baked* table; the host's (possibly newer) data is irrelevant. No re-sort, no corruption.
- **A reference-mode file** re-imports + hash-checks on open (§3): match → use; mismatch →
  hard-fail (the controlled-environment trade).
- **Adopting newer host/Unicode data is explicit and opt-in.** A re-load + `REINDEX` (or an
  `ALTER … COLLATION UPGRADE`-style op, named at the slice) re-imports the collation, rebuilds
  the affected indexes against the new table, and updates the stamp. The user chooses when to
  pay the re-sort; nothing forces it.
- **A tampered baked table or a mismatched reference** surfaces as a structured error on read
  ([storage.md §6](storage.md)) — never silently wrong rows.

This inverts PostgreSQL's posture: PG's default path is "the OS/library moved, your index may be
corrupt, here is a `collversion` warning"; jed's default (baked) path is "nothing moved, the
file carries its own order," and the version move is the rare, deliberate act.

## 13. Sizes

The footprint that shaped §2/§3 (orders of magnitude):

| | Size |
|---|---|
| **Binary** (collation data) | **~0** — jed vendors no tables; only the compiler + executor code |
| **Baked root-bearing collation, in file** | compiled table **< 1 MB** → **~0.3–0.5 MB** under the LZ4 codec |
| **Baked locale tailoring, in file** | **~1–100 KB** (a diff over the root) |
| **Reference-mode collation, in file** | **tens of bytes** (name + version + hash + optional description) |
| **`C`** (binary or file) | **0 bytes** (table-free; the default) |
| **CJK (Han) tailoring** | the one outlier — low **single-digit MB** (deferred, §14) |
| **Full ICU `.dat`** (for contrast) | ~30 MB — never shipped or baked; we own our surface |

So the **binary stays lean regardless of how many locales exist**: data lives in the host and,
when used, in the file. A baked file pays a one-time ~0.3–0.5 MB for
the root plus a few KB per locale; a reference-mode file pays tens of bytes; a `C`-only database
pays nothing.

## 14. Deferred narrowings and slice plan

**Slice 1 — the compile + serialize + execute core — is itself decomposed into vertical
sub-slices**, each independently testable (CLAUDE.md §10), in dependency order:

- **1a — byte-format foundation** ✅ *landed*: `spec/collation/` — the definition format (DUCET
  `allkeys.txt` + LDML, §9), the compiled-table layout, the `.coll` artifact, and the sort key
  ([../collation/README.md §1–§4](../collation/README.md)), plus the dev fixtures
  (`dev-root.allkeys` + `dev-nordic.ldml`). Spec/data only, no core code.
- **1b — `CompileCollation` + UCA executor**, all three cores (compiler-first, §6): parse a
  definition → compiled table; generate sort keys; `SaveCollation`/`OpenCollation` round-trip.
  Host-free, verified by the compiler + sort-key vectors (§9/§10) and the artifact round-trip. No
  SQL surface, no persistence — the riskiest cross-core piece, isolated.
- **1c — first end-to-end (in-memory)**: `COLLATE` grammar + resolver (collation derivation,
  `42P22`, `42704`), `db.ImportCollation` into an **in-memory** database (no format change yet —
  in-memory `commit` is a no-op, [api.md](api.md)), `ORDER BY … COLLATE` + collated comparison;
  the corpus fixture-load directive that drives it deterministically (§10). The "it's alive"
  milestone for collation.
- **1d — on-disk baking**: the `format_version` bump + the `entry_kind` 3 snapshot + the
  per-database default-collation field (§5); `db.ImportCollation` (baked) persisting,
  `db.ExportCollation`, `db.SetDefaultCollation` + un-annotated-column inheritance (§1); the
  provenance description persisted (§1/§5); a golden DB (`rust == go == ts == ruby`, §10).
- **1e — collated keys**: the sort-key key encoding (§8) as a new [encoding.md](encoding.md)
  sub-section, a collated text `PRIMARY KEY` / index / `UNIQUE`, byte fixtures + a golden with a
  collated index.

**Later follow-ons** (each its own slice, after slice 1):

- **Host seam — `ExtractHostCollation`** (§4) — per core, per platform; tested per core (§10);
  auto-fills the provenance description. The corpus never uses it.
- **Reference mode** (name+hash, re-import + hash-check on open, §3) — the small-footprint
  opt-out.
- **Curated tailorings** (`en-US`, `de`, `fr`, `es`, `sv`, `da`, §9) and the **version-pinned
  real DUCET** (replacing the dev fixture) — fixtures + conformance entries; no engine change.
- **`LIKE` / pattern matching under a non-`C` collation** (§7) — lift the byte-semantics
  narrowing.
- **CLDR `shifted` variable weighting** per locale (§6) — refine away `non-ignorable`, pinned
  to the oracle.
- **Nondeterministic collations** (case/accent-insensitive *equality*, §6) — the big one:
  forces the UNIQUE-collision / DISTINCT / GROUP BY / hashing / pattern paths to be handled.
- **CJK (Han) collation** (§13 outlier) — a multi-MB tailoring; gate explicitly on the
  per-file footprint trade.

A SQL surface for loading (e.g. a privileged `CREATE COLLATION … FROM HOST | FROM DEFINITION`)
is a possible later addition, but the **primary surface is the host API** (§1/§11): loading
reaches the host environment and so must stay off the untrusted-SQL surface.

## 15. Divergences from PostgreSQL

Recorded per CLAUDE.md §1:

- **Default column collation is the per-database default stored in the file** (itself `C` at
  creation, settable, §1) — **not** the host `LC_COLLATE` and **not** a hard-wired constant.
  (Reason: determinism + no ambient-locale dependency, CLAUDE.md §8/§10.)
- **Collations must be explicitly loaded before use** (§1/§4); PG resolves collations from the
  OS/locale environment implicitly. (Reason: make the host boundary explicit and keep every use
  pure, §2/§11.)
- **jed vendors no collation tables in the binary**; data is host-imported or
  definition-supplied and compiled into jed's own table (§2/§9). (Reason: binary footprint +
  a growable, non-wired-in locale set.)
- **Collation data is baked into the database file by default** (§3); PG stores only a
  `collversion` string and relies on the host library at runtime. The central divergence and the
  reason jed has no collation-corruption-on-upgrade failure mode. **Reference mode** (§3) is the
  opt-in that approaches PG's host-dependent posture, but with a hard hash check.
- **Collated indexes store UCA sort keys** (memcmp-ordered, §8); PG stores the original and
  compares with a runtime comparator. (Reason: the single-`memcmp`-order storage contract,
  [encoding.md §1](encoding.md).)
- **Only deterministic collations in the first slice** (§6/§14); PG ships nondeterministic ICU
  collations from the start.
- **No implicit `CREATE COLLATION` on the untrusted surface** (§14); loading is a privileged
  host op.

Where jed *does* implement a collation, its **ordering matches PostgreSQL's same-locale ICU
ordering** for the supported levels (the conformance default, §10) — the divergences above are
about *which* collations exist, *where* their data lives, *how* it is loaded, and *how* keys are
stored, not about getting a supported locale's order wrong.
