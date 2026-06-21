# Collation — design

> Linguistic (locale-aware) collation for `text`: dictionary-style ordering (`ä` near `a`,
> not after `z`) layered on the existing UTF-8 `text` type via a `COLLATE` clause, a per-column
> collation, and a **per-database default collation** recorded in the file. The engine **owns the
> collation algorithm** (a hand-written Unicode Collation Algorithm, UTS #10, in every core) and
> **vendors the compiled collation tables into each core** at an **embedder-chosen footprint
> tier** — `C`-only / everything-except-CJK / everything (§13). The database file **never
> snapshots collation data**: it **references** the collations it uses by name and records the one
> **`(unicode_version, cldr_version)`** its keys were built under (§5); version skew between that
> pin and a binary's vendored data is resolved by the **graded, legible open-time verdict** of
> [compatibility.md](compatibility.md) (full / read-only heap-scan / legible refusal), not by
> carrying tables in the file. The vendored tables are produced by a **build-time pipeline** (raw
> DUCET/CLDR → canonical jed definitions → compiled `.coll` artifacts, §9) and embedded
> **identically** into every core; `ExtractHostCollation` and `CompileCollation` are **build-time
> tooling, compiled out of the production engine** (§4/§9). The **production** collation surface is
> therefore small: read a vendored table (`OpenCollation`) and run the executor; the SQL surface
> only ever *references* a vendored collation by name (§1). This doc is the contract all three
> cores implement in lockstep (CLAUDE.md §2). The `text` type and the `C`-collation baseline are in
> [types.md §11](types.md); the key-encoding rule in [encoding.md §2.4](encoding.md); the
> catalog/byte layout in [../fileformat/format.md](../fileformat/format.md); the LZ4 codec that
> compresses the vendored `.coll` artifact in [large-values.md](large-values.md); the host-seam pattern in
> [hosts.md](hosts.md) and [entropy.md](entropy.md); the determinism stance in
> [determinism.md §3](determinism.md); the cost contract in [cost.md](cost.md); the
> data-over-code framing in [extensibility.md §4.1](extensibility.md).
>
> **Status: design ratified; slices 1a–1e landed the algorithm, sort-key encoding, and SQL surface
> in all three cores — under an earlier baked-by-default, host-extracted persistence model now
> REVISED here.** This doc pivots collation to **vendored-tier + reference-only**: collation data is
> compiled into each core at an embedder-chosen footprint tier (§13) and the file references it by
> name + `(unicode_version, cldr_version)` (§5), **never** snapshotting a table. The change brings
> collation into line with [timezones.md](timezones.md) (vendor a pinned version, reference it in the
> file) and lets it reuse [compatibility.md](compatibility.md)'s manifest + graded verdict for
> version skew, in place of the baked / host-reimport machinery slices 1d/1e shipped.
>
> - **Retained from 1a–1e:** the UCA compiler + executor (§6), the sort-key key encoding
>   ([encoding.md §2.12](encoding.md), §8), and the SQL surface — the `COLLATE` postfix operator,
>   `ORDER BY … COLLATE`, per-column collation with default inheritance frozen at `CREATE TABLE`, the
>   derivation rules and their errors (`42704`/`42804`/`42P21`/`42P22`), and the `collate` cost unit.
> - **Superseded** (removed at the next format-touching slice): on-disk **baking** — the
>   `format_version` 17 `entry_kind` 3 baked snapshot shrinks to a name+version **reference entry**
>   (§5); the runtime load/import/export lifecycle (`db.ImportCollation` / `ExportCollation` /
>   `LoadHostCollation`); and **the host seam in the running engine** — `ExtractHostCollation` and
>   `CompileCollation` become **build-time tooling** that regenerates the vendored `.coll` set (§9),
>   compiled out of the production engine.
> - **Not yet built:** the build-time vendoring pipeline, the real version-pinned DUCET + curated
>   tailorings (still the §14 follow-on), and the [compatibility.md](compatibility.md)
>   manifest/verdict that reference-only leans on — so until those land, `C` remains the only
>   collation a production build actually carries.
>
> Two foundational choices are unchanged: the definition format is the **UCA/CLDR standards** (DUCET
> `allkeys.txt` + LDML), and the `.coll` **compiled artifact is the one shared cross-core form** every
> core embeds and reads (`OpenCollation`) — never a per-core compile (§9, the [timezones.md §3.3](timezones.md)
> compiled-TZif precedent).

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

A collation is **vendored into the binary** (§9/§13) — there is **no runtime load step.** A
collation name is usable in a database iff the engine's build tier (§13) carries it; naming one the
build does not include is **`42704`** (`undefined_object`), the same code an unknown array/range
element type raises. This replaces the earlier explicit-load lifecycle (`db.LoadHostCollation`, now
build-time only — §4/§14): pushing the host environment entirely to build time means **every runtime
*use* is pure** (§11) and there is **no host seam left in the running engine.**

```
// production host API (privileged — not untrusted SQL, §11):

db.SetDefaultCollation("en-US")   // set the per-database default (must be vendored in this build)
db.DefaultCollation()             // read the per-database default
db.Collations()                    // introspect vendored collations: name, (unicode, cldr) version, description

// build-time tooling ONLY — compiled out of the production engine (§4/§9):
//   ExtractHostCollation(name)        host ICU/CLDR  → canonical jed definition
//   CompileCollation(name, defReader) definition     → compiled `.coll` table
//   SaveCollation / OpenCollation     `.coll` artifact serialize / deserialize
// These regenerate the vendored spec/collation/ data; production reads the vendored `.coll`
// via OpenCollation at startup and never compiles or reaches the host.
```

```sql
-- SQL surface (a collation just needs to be vendored in the build):
CREATE TABLE people (id i32 PRIMARY KEY, name text COLLATE "en-US")
CREATE INDEX ON people (name)                   -- ordered by the column's en-US collation
SELECT name FROM people ORDER BY name             -- en-US order
SELECT name FROM people ORDER BY name COLLATE "C" -- override: byte order
SELECT 'ä' < 'z' COLLATE "de"                      -- per-expression collation (de must be vendored)
```

- **`COLLATE "name"`** is a postfix operator on a text expression yielding the same value with
  a different collation for the surrounding comparison/sort. It binds at the **postfix / typecast
  level** — the same rung as `::` / `[]` / `.field`, so **tighter than `||` and the comparison
  operators** ([grammar.md §47](grammar.md), PostgreSQL precedence: `a || b COLLATE "x"` is
  `a || (b COLLATE "x")`, `'a' < 'b' COLLATE "x"` is `'a' < ('b' COLLATE "x")`). Naming a
  collation **not vendored in the build** is **`42704`** (`undefined_object`), the same code
  arrays/ranges raise for an unknown element type; applying `COLLATE` to a non-collatable
  (non-text) expression is **`42804`** (`datatype_mismatch`, PG-matching). The name is a quoted
  identifier — a non-`C` type is a 1c-only narrowing on which the version-pinned real collation
  set later builds. *(Slice 1c implements COLLATE at the postfix rung; an earlier draft of this
  doc said "looser than `||`", which mis-stated PG — corrected here.)*
- **Collation names are quoted identifiers** (they contain hyphens): `"C"`, `"en-US"`, `"de"`,
  `"sv"`. `"C"` is always available; every other name must be **vendored in the build** (§13).
- **Per-database default collation** (§3). Every database has a default collation **recorded in
  its file** (by name + version, never as data); an un-annotated `text` column uses it. It is
  **`C` at creation** and can be set to any *vendored* collation via `db.SetDefaultCollation`.
  This is the answer to "don't hard-code
  `C`, and don't depend on the host `LC_COLLATE`": the default is a deliberate, persisted,
  per-database choice — not an ambient host locale, not a wired-in constant.
- **Per-column collation.** A `text` column may carry an explicit collation
  (`name text COLLATE "en-US"`); absent a clause it inherits the **database default**. The
  column collation is the default for every comparison / `ORDER BY` / `DISTINCT` / `GROUP BY` /
  `PRIMARY KEY` / `UNIQUE` / index over that column; an explicit query `COLLATE` overrides it
  for that expression.
- **Collation derivation in expressions** follows PG's rules: an explicit `COLLATE` is
  *explicit*; a column reference is *implicit*; a literal has no collation and takes its
  neighbour's. Two **conflict** codes, both PostgreSQL's: combining two different **explicit**
  collations in one operator is **`42P21`** (`collation_mismatch`, "collation mismatch between
  explicit collations"); combining two different **implicit** collations is **`42P22`**
  (`indeterminate_collation`, "could not determine which collation to use"), resolved by an
  explicit `COLLATE`. **Both are reachable since slice 1d** — a column reference's implicit collation
  is its frozen collation, with **`C` a distinct implicit collation** (so a `C` column vs an `en-US`
  column conflicts — PG-matching). The conflict is derived for **all** comparison ops including
  `=`/`<>` (PG raises it regardless), even though jed's `=`/`<>` ignore the collation at eval (byte
  equality, §7). (In slice 1c only `42P21` was reachable — every column was implicitly `C`. An earlier
  draft named `42P22` for the explicit case — corrected: PG raises `42P21` there.)
- **Provenance + introspection.** Each vendored collation carries an optional, human-readable
  **description** recording where it came from — auto-filled at **build time** by
  `ExtractHostCollation` with the core/OS/library identity (e.g. `Go 1.26.3 / Linux 7.1 / ICU 73`),
  baked into the `.coll` artifact (§4), travelling with the vendored table, and surfaced by
  introspection (`db.Collations()` → name, `(unicode, cldr)` version, description). It is
  descriptive metadata only — **excluded from the content hash** (§4), so it never affects ordering
  or dedup.

## 2. The fixed architecture: jed owns the algorithm; tables are vendored, not host-read

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

So jed **vendors the compiled collation tables into each core** (§9/§13) and reads them at
runtime — the host environment is consulted **only at build time**, to *produce* the vendored
data, never by the running engine. (An earlier model vendored nothing and read the host at
runtime via an explicit load; that left a non-deterministic seam in the engine and made every
file carry its own tables — both now removed.) The architecture has three layers; **only the
lower two ship in the production engine**, and they are the cross-core contract:

1. **The jed collation table** — jed's own compiled, executable form (collation elements +
   multi-level weights, §6), vendored into the binary and read at startup. What the executor runs
   on.
2. **The executor** (table → ordering / sort key, §6) — **jed-owned, hand-written per core,
   spec'd** (CLAUDE.md §5 forbids codegenning it), **cross-core byte-identical given identical
   input**, verified by byte fixtures (§10), exactly the composite/array precedent
   ([extensibility.md §4.1](extensibility.md)). This is the **whole production collation code**:
   deserialize a vendored `.coll` (`OpenCollation`) and run the executor.
3. **The build-time pipeline** (§9) — the **compiler** (`CompileCollation`: canonical definition →
   jed table) and **the host seam** (`ExtractHostCollation`: raw host ICU/CLDR → definition). These
   *produce* the vendored `.coll` set and are **compiled out of the production engine.** The
   compiler stays hand-written + cross-core-tested (its vectors, §10) so any core's build can
   regenerate the pinned `.coll` byte-identically — but no core invokes it at runtime.

> **The determinism boundary, stated once:** cross-core byte-identity is a property of *a vendored
> jed table + the executor*. The table is **the same `.coll` bytes embedded into every core** (§9),
> so a query never observes any host variation — it runs over identical vendored bytes. All the
> messy host reading happens **once, at build time** (`ExtractHostCollation` → `CompileCollation` →
> the committed `.coll`), behind a CI reproducibility check (§9/§10). This is the same shape as the
> storage seam (fixed behavior over supplied bytes), not the clock seam (a per-query draw) — and the
> seam now sits at *build time*, not at file-open.

## 3. Where the data lives: vendored in the binary, referenced in the file

The collation table lives in **the binary** (vendored, §9/§13), and the database file **references
it** — it never carries the table:

- **Vendored (the only mode).** The compiled jed table is embedded in the engine at an
  embedder-chosen footprint tier (§13). The runtime reads it; the file records only **which
  collations it uses** (by name) and the one **`(unicode_version, cldr_version)`** its keys were
  built under (§5). Since the data is small and present in every non-`C` build anyway (§13),
  storing it per-file would not shrink the distribution — it would only add a second copy and a
  cross-version-skew hazard (a file accumulating collations from different vendored versions). So
  jed does not.
- **Skew is handled by the verdict, not the file.** A file pinned to `(unicode, cldr) = X` opened
  on a binary whose vendored data is also `X` → full read-write. On a binary at a *different*
  version (or a *lower tier* that lacks the collation) → the **graded open-time verdict** of
  [compatibility.md](compatibility.md): **read-only heap-scan** (values are version-independent,
  [compatibility.md §4.1](compatibility.md); the suspect collated index is not used for
  acceleration and not maintained until a migration rebuilds it) or, for an entirely absent
  read-required dependency, a **legible refusal** naming the missing collation + version. This
  *replaces* the old baked-vs-reference choice and its host-reimport hash check.

Crucially, this is **not** PostgreSQL's host-dependent posture: the reference is to **vendored,
pinned, version-stamped** data that moves only on a discrete jed release — not to the host OS's
drifting ICU/glibc. A file is fully portable to **any binary of the same tier + Unicode version**,
and degrades *legibly* (never silently-wrong rows) elsewhere. A database that uses only `C` (the
creation default) carries **zero** collation data and needs **zero** vendored tables.

Every collated index records the `(name, unicode_version, cldr_version)` it was built under (the
stamp). It is what the open-time verdict checks against the binary's vendored version and what makes
a deliberate re-collation (§12) a *controlled* event.

## 4. The build-time pipeline and the production surface

The lifecycle splits cleanly in two: a **build-time pipeline** that produces the vendored `.coll`
set, and a **production surface** that only ever *references* a vendored collation by name. A
**`Collation`** is the unit the pipeline manipulates — a jed table (§6) plus its metadata (`name`,
`(unicode, cldr)` version, content `hash`, optional `description`).

### 4.1 Build-time pipeline (compiled out of the production engine)

These run when the vendored `spec/collation/` data is **regenerated** — typically only on a Unicode
version bump or when a tailoring is added — never in a shipped engine:

- **`ExtractHostCollation(name) -> definition` — host-dependent, build-time only.** On a machine
  that has ICU/CLDR, read the host's collation **data** (ICU bundles, system locale data) and
  normalize it into a canonical jed **definition** (§9); where none is readable, fall back to
  probing the host collator (approximate, last resort). It **auto-fills the `description`** with the
  core/OS/library identity (e.g. `Go 1.26.3 / Linux 7.1 / ICU 73`). Because it depends on the host
  library/version it is **not cross-core-deterministic** — which is exactly why its *output* is
  pinned (the committed definition + `.coll`) and re-derivation is gated by a CI diff, not trusted
  per-run.
- **`CompileCollation(name, definitionReader) -> Collation` — deterministic.** Compiles a canonical
  **definition** (§9 — UCA root weights + LDML tailoring) into a jed table that is **byte-identical
  on every core**. Run **once** in the pipeline to produce the committed `.coll`; its cross-core
  vectors (§10) guarantee any core's build reproduces the same bytes (so there is no reference
  implementation — CLAUDE.md §2).
- **`SaveCollation(coll, writer)` / `OpenCollation(reader)`** — the artifact codec. `SaveCollation`
  writes the **`.coll` artifact** (magic + format version + `name` + `version` + `hash` +
  `description` + the compiled table, table LZ4-compressed [large-values.md](large-values.md));
  `OpenCollation` is its exact inverse, byte-identical on every core (§10). The `.coll` is the **one
  shared cross-core form**: the committed `spec/collation/` fixtures (§9) and the bytes embedded into
  each core are the same `.coll`. **`OpenCollation` is the one pipeline routine that also ships in
  production** (§4.2, the read path); `SaveCollation` and the two producers above are build-time only.

### 4.2 Production surface

The shipped engine carries **`OpenCollation` + the executor only** (§2 layer 2). At startup it reads
the vendored `.coll` set into in-memory tables; thereafter:

- **`db.SetDefaultCollation(name)` / `db.DefaultCollation()`** set/read the per-database default
  (the name must be vendored in this build, else `42704`).
- **`db.Collations()`** introspects the vendored set: `name`, `(unicode, cldr)` version,
  `description`.
- the SQL surface (`COLLATE`, per-column collation, `ORDER BY … COLLATE`) **references** vendored
  collations by name.

There is **no `db.ImportCollation` / `ExportCollation` / `LoadHostCollation`** in production — no
runtime path constructs, loads, or persists a collation table. **`C` is never vendored or
referenced** — it is table-free and built in.

## 5. On-disk representation

The file records **which collations it uses and the one version they were built under — never a
table.** Two pieces, both small:

- **The per-database default collation** (§1) is the **`is_default` flag bit on the reference entry
  it names** (`C` ⇒ no entry flagged). jed's catalog packs whole kind-tagged entries (no free-form
  header stream) and the meta page is a fixed-width, CRC-protected layout, so a flag bit on the
  (always-present, since a non-`C` default must be a referenced collation) entry is the clean home —
  no meta-layout change.
- **A referenced collation is a kind-tagged catalog entry** (`entry_kind` 3, after `0` table, `1`
  composite type, `2` sequence — [format.md](../fileformat/format.md)), emitted *composite types →
  sequences → collations → tables* so a table/index entry that references one is read after it. The
  entry holds **only metadata** — no table:
  - the **name** (`"en-US"`),
  - the **`(unicode_version, cldr_version)`** the keys were built under (the stamp, §3),
  - the optional **provenance description** (§1) — a length-prefixed UTF-8 string,
  - the **`is_default`** flag bit.

  It carries **no compiled table and no LZ4 blob** — the table is vendored (§2/§9). (An optional
  content **hash** of the vendored `.coll` may be recorded as a cheap open-time integrity check
  against a mis-built binary; it is *not* load-bearing for correctness the way the old
  host-reimport hash was, since `(name, unicode, cldr)` already uniquely identifies the committed
  `.coll`.) **This supersedes the `format_version` 17 baked snapshot** (the LZ4-compressed table is
  removed); the shrink to a metadata-only entry is itself a `format_version` bump at the
  implementation slice.

The per-column collation rides the slot [format.md](../fileformat/format.md) already reserves
for it (the per-column flags + typmod-adjacent field, where `varchar(n)` and the composite/array
type descriptors live). An **index entry** records the collation it was built under by
`(name, unicode_version, cldr_version)`.

The on-disk bytes are now version-independent of any table: every core that **vendors the same
`(unicode, cldr)`** computes identical sort keys (§8) → a byte-identical collated B-tree in the
goldens (§10). A core at a *different* vendored version (or a lower tier) does not silently produce a
divergent tree — it hits the open-time verdict (§3/§12, [compatibility.md](compatibility.md)).

## 6. The algorithm: a compiler and an executor

Each core implements **two** hand-written collation routines (CLAUDE.md §5 forbids codegenning
either), both deterministic and cross-core byte-identical given identical input. They sit on
opposite sides of the build/runtime line (§4): the **executor ships in the production engine**; the
**compiler is build-time tooling** (§9) — still hand-written + cross-core-tested per core (its
vectors, §10) so any core's build can regenerate the pinned `.coll`, but compiled out of a shipped
engine, which reads already-compiled tables via `OpenCollation`.

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
- **`COLLATE` conflict** (`42P21` explicit-mismatch this slice; `42P22` implicit conflict at 1d),
  **not-vendored collation** (`42704`), and **non-text COLLATE** (`42804`) are the new errors in this
  path.
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
The sort key is produced by the **vendored** table (§2/§9), so every core at the same vendored
`(unicode, cldr)` version emits identical key bytes → byte-identical collated B-trees.

**Two narrowings the slice-1e key path carries** ([encoding.md §2.12](encoding.md)), both relaxable:

- **Point-lookup pushdown is deferred for a collated key.** A collated PK/index `WHERE k = 'x'` /
  `k < 'm'` **full-scans + residual-filters** — correct, just unindexed, the same posture as a range
  container key ([encoding.md §2.11](encoding.md)). The planner already excludes a *collated*
  comparison from a byte-range index bound (it would compute a `C`-byte bound against a
  collation-ordered B-tree — wrong), so this falls out for free: a `C` text key still pushes down; a
  non-`C` one does not. (Equality pushdown is sound in principle — the sort key is injective via the
  identical level — and is the obvious follow-on.)
- **One uniform component codec.** A collated text key component is the **full** sort key (identical
  level included) in every position — PK body, secondary-index entry, `UNIQUE` prefix. The
  alternative `sort_key ‖ pk` (no identical level) for a secondary index is *also* correct but is not
  taken: one codec, no special-casing, at the cost of a few redundant trailer bytes in the index.

## 9. The data: the build-time pipeline and the vendored artifact

The pipeline turns raw Unicode/CLDR data into the **one shared `.coll` form** every core embeds.
It runs **at build time** — none of its first two stages ships in the production engine (§4.1):

```
raw Unicode data:  DUCET allkeys.txt  +  CLDR LDML tailorings        (pinned: unicode_ver, cldr_ver)
        │   ExtractHostCollation / a normalizer   (build-time tooling — host-dependent)
        ▼
canonical jed definitions   spec/collation/<ver>/*.allkeys + *.ldml   (committed source, auditable)
        │   CompileCollation  (run ONCE in the pipeline — cross-core-deterministic, §6)
        ▼
compiled artifacts          spec/collation/<ver>/*.coll              (committed, byte-pinned golden)
        │   each core EMBEDS these bytes at its chosen tier (§13)
        ▼
in-binary collation data    Rust include_bytes! / Go embed / TS bundled asset
        │   OpenCollation at startup  (production — the ONLY pipeline stage that ships)
        ▼
in-memory jed tables → the executor (§6)
```

Two properties make this safe and cheap:

- **Compile once, embed identical bytes.** The `.coll` is produced by a single pipeline run and
  committed as a byte-pinned golden; every core embeds the *same bytes* and reads them with
  `OpenCollation`. Cross-core byte-identity is then **trivial** (same input bytes) rather than
  contingent on every core's compiler agreeing — exactly the [timezones.md §3.3](timezones.md)
  reasoning for vendoring compiled TZif rather than running `zic` per core. The compiler still ships
  cross-core vectors (§10) so **any** core's build can regenerate the pinned `.coll` byte-identically
  (no reference implementation, CLAUDE.md §2), behind a **CI reproducibility check** that re-runs the
  pipeline and diffs against the committed `.coll`.
- **The host is read only at build time.** `ExtractHostCollation`'s non-determinism is contained by
  pinning its output (the committed definition + `.coll`), never by trusting a per-run extraction.

**`spec/collation/`** (a spec data directory parallel to `spec/encoding/`) holds the **byte-format
spec, fixtures, and verification vectors** — *repo data* — that double as the **source the cores
vendor from**. The byte formats are pinned in [../collation/README.md](../collation/README.md) (the
definition format §1, the compiled table §2, the `.coll` artifact §3, the sort key §4). It holds:

- the **definition format spec** (DUCET `allkeys.txt` subset + LDML tailoring subset) and the pinned
  `(unicode_version, cldr_version)` of the real root when it lands,
- the **definition fixtures** (the dev `dev-root.allkeys` + `dev-nordic.ldml`; the curated `en-US`,
  `de`, `fr`, `es`, `sv`, `da` set — the last two for the sharp `å ä ö`/`æ ø` after-`z` cases — as a
  follow-on),
- the **compiled `.coll` artifacts** those definitions produce — *both* the corpus's deterministic,
  host-free collation source *and* the bytes embedded into each core,
- **compiler vectors** — `(definition fixture) → (expected `.coll` / jed table bytes)`,
- **executor / sort-key vectors** — `(collation, string) → (sort-key bytes)`, the §8 byte-fixture
  pattern (CLAUDE.md §8) and the primary cross-core contract for the algorithm.

So both the corpus and the production cores load collations *deterministically* from the committed
`.coll` — never `ExtractHostCollation`, never a runtime compile, independent of any host.

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
- **A golden file containing a referenced-collation catalog entry + a collated index** extends the
  byte-exact on-disk round-trip (`rust == go == ts == ruby`, CLAUDE.md §8) — pinning the
  metadata-only entry (§5) and the collated B-tree's key bytes (produced by the **vendored** `.coll`)
  in one fixture. The vendored `.coll` itself is pinned separately by the compiler vectors above.
- **Conformance entries** drive collation by **referencing a vendored `.coll`** (the committed
  fixture, never `ExtractHostCollation`), so all three cores read the identical table → identical
  orderings; oracle-checked against `postgres:18` where jed matches PG and overridden-with-reason
  where it diverges (§15).
- **`ExtractHostCollation` (the build-time host seam) is tested per core**, against that core's own
  host — the [conformance.md](conformance.md)/CLAUDE.md §10 carve-out for "what the corpus cannot
  express" (host introspection / platform-specific behavior), since the host path is
  *deliberately* not cross-core-identical (§2/§4). It is a *tooling* test, not a production-engine one.

## 11. Untrusted-query safety, cost, and the determinism ledger

- **No host seam in the running engine; using is pure** (CLAUDE.md §13). This is *stronger* than
  the old "loading is a privileged host op" stance: there is **no runtime load at all.** The only
  thing that ever touched the host (`ExtractHostCollation`) is **build-time tooling, compiled out of
  the production engine** (§4.1), so an adversarial query has nothing to trigger — it can only *use*
  a vendored collation by name, or get `42704`. Using a collation is **pure** — a string and a
  vendored table in, a sort key out; no host reach, no I/O, no nondeterminism. (`db.SetDefaultCollation`
  and introspection are privileged host-API ops over already-vendored data, never on the untrusted
  surface.)
- **Bounded cost.** Sort-key generation is metered by a `collate` cost unit per code point
  (table-bounded lookups, bounded contractions/expansions), so a collated comparison over a large
  input is cost-ceilinged ([cost.md](cost.md)). The unit landed in **1c**, charged at the
  **comparison-operator evaluation** site — the deterministic, cross-core-identical metering point:
  each ORDERING comparison (`< <= > >=`) under a collation charges `collate × (codepoints(lhs) +
  codepoints(rhs))`. `=`/`<>` charge nothing here (deterministic-collation equality is byte-equality,
  §7). The **`ORDER BY` sort itself stays unmetered**, like every sort ([cost.md §3](cost.md),
  [spill.md §6](spill.md)); its input cardinality is bounded by the upstream `storage_row_read` /
  `row_produced`, and its decorate sorter builds each row's sort key exactly once. (The original plan
  named ORDER BY as a metering site; the comparison evaluator is the one deterministic, meterable
  point — the set-operation sort path carries no `Meter` at all — so the spec is refined to charge
  there, which is consistent with sorts being unmetered.)
- **Collation *use* stays OUT of the determinism ledger.** Because a query runs over a **vendored**
  table with a jed-owned executor, it is a deterministic function of its inputs — precisely the
  outcome [determinism.md §3](determinism.md) demands. Which collations a build vendors is a
  build/configuration boundary (like *which file you opened*), not a query-time draw, so it needs no
  ledger entry either: no query observes the vendoring (§2).

## 12. Migration and version adoption

The reference-only model (§3) keeps a jed upgrade from *silently* breaking a file, while pinning +
the graded verdict make any genuine version move legible:

- **Same vendored version → opens fully.** A file pinned to `(unicode, cldr) = X` on any binary that
  vendors `X` at a covering tier (§13) reads-writes normally — collated structures use the vendored
  table, no re-sort.
- **Different vendored version, or a lower tier → graded verdict, never wrong rows.** A binary at a
  *different* `(unicode, cldr)` (or a tier lacking the collation) does **not** silently re-order:
  the open-time verdict ([compatibility.md §7–§8](compatibility.md)) degrades the affected object to
  **read-only heap-scan** — values are version-independent ([compatibility.md §4.1](compatibility.md)),
  so the base table reads correctly; the suspect collated index is not used for acceleration and not
  maintained — or, for an entirely absent read-required dependency, **refuses legibly** naming the
  missing collation + version. The optional `.coll` hash (§5) catches a *mis-built* binary that
  vendors wrong bytes under the right version label.
- **Adopting a newer Unicode/CLDR version is explicit and opt-in.** Running a binary built on the new
  version + a `REINDEX` (or an `ALTER … COLLATION UPGRADE`-style op, named at the slice) rebuilds the
  affected indexes against the new vendored table and re-pins the stamp. The user chooses when to pay
  the re-sort; nothing forces it. (This is the concrete cost reference-only adds over the old
  bake-forever model: after a jed Unicode bump an old file is **read-only until REINDEX** on the new
  binary, rather than fully usable forever — accepted because the data stays readable, the
  degradation is legible, and collation versions move rarely.)

This is still a sharp contrast with PostgreSQL: PG depends on the **host OS's** ICU/glibc, which
drifts *silently* under an OS upgrade and may corrupt an index with only a `collversion` warning.
jed's reference is to **vendored, pinned, version-stamped** data that moves only on a discrete jed
release, and every move is caught by the verdict — so jed still has **no silent-corruption failure
mode**; it trades bake's "works fully forever" for "degrades legibly, migrate deliberately."

Collation version skew is one instance of a **general** problem — stored bytes produced by a
versioned computation (a collation, the IANA tzdata version behind a tz-derived key, a built-in
function in a `DEFAULT`/functional index/generated column, a stored view). Reference-only makes
collation a **clean instance** of the cross-cutting model in
[compatibility.md](compatibility.md) — a per-file Unicode-version pin, a requirements manifest, and a
graded read-only heap-scan degradation — alongside [timezones.md](timezones.md), which already
vendors + references its data the same way. That model is still an **unratified proposal**; until it
is adopted the on-disk policy remains clean-break exact-version
([../fileformat/format.md](../fileformat/format.md)), and reference-only collation lands together with
(or behind) the manifest it leans on (§14).

## 13. Sizes — the three vendoring tiers

The footprint is now a **binary build choice**, not a per-file cost (§3). The embedder picks one of
three **cumulative tiers** at build/link time; the file carries only metadata regardless.

| Tier | Vendored collation data | Compiled size (LZ4) |
|---|---|---|
| **1 — `C`-only** | none (the `C` baseline is table-free) | **0 bytes** |
| **2 — everything except CJK** (the common build) | root DUCET + all non-CJK tailorings | **< ~2 MB** (root ~0.3–0.5 MB + a few KB per locale) |
| **3 — everything** | tier 2 + the CJK (Han) tailoring | tier 2 + low **single-digit MB** (the one outlier) |
| *(in file, any tier)* | none — name + `(unicode, cldr)` + optional description/hash | **tens of bytes** |
| *(for contrast) full ICU `.dat`* | never shipped — we own our surface | ~30 MB |

Notes that shape the tiers:

- **The tiers gate only the CLDR *collation* tailorings.** The universal Unicode **property tables**
  for `lower`/`upper`/`normalize`/regex are a *separate, smaller* vendored dataset on the **same one
  `(unicode_version)`** axis, included whenever those functions are built — so even a tier-1 (`C`-only)
  collation build can carry `lower()`. **One vendored Unicode version per binary** spans both.
- **The file's cost is flat.** A `C`-only database carries zero collation data; any other database
  carries only the **reference metadata** (tens of bytes per distinct collation), never a table.
- **The web/OPFS target benefits most** — a browser build can ship tier 1 (or tier 1 + property
  tables) and avoid shipping megabytes of collation it does not use, while a server build ships
  tier 2/3. The tier maps onto the existing capability-tier system (CLAUDE.md §7).

## 14. Deferred narrowings and slice plan

**Slice 1 — the compile + serialize + execute core — is itself decomposed into vertical
sub-slices**, each independently testable (CLAUDE.md §10), in dependency order:

- **1a — byte-format foundation** ✅ *landed*: `spec/collation/` — the definition format (DUCET
  `allkeys.txt` + LDML, §9), the compiled-table layout, the `.coll` artifact, and the sort key
  ([../collation/README.md §1–§4](../collation/README.md)), plus the dev fixtures
  (`dev-root.allkeys` + `dev-nordic.ldml`). Spec/data only, no core code.
- **1b — `CompileCollation` + UCA executor**, all three cores (compiler-first, §6) ✅ *landed*:
  parse a definition → compiled table (`impl/{rust,go,ts}/…collation…`); generate sort keys;
  `SaveCollation`/`OpenCollation` round-trip. Host-free, verified by the populated compiler +
  sort-key vectors ([../collation/vectors/](../collation/), §9/§10) and the artifact round-trip;
  byte-identical across cores by construction. No SQL surface, no persistence — the riskiest
  cross-core piece, isolated. The `collate` cost unit (§11) is **deferred to 1c** (1b's `sortKey`
  is a pure function with no metering site). One spec refinement made here: a definition is a
  **single line-dispatched stream** (allkeys lines vs `&`-led LDML rule lines), so a single
  `CompileCollation(name, reader)` consumes a root followed by its tailorings
  ([../collation/README.md §1](../collation/README.md)); the dev tailoring weight allocator is
  pinned in [../collation/README.md §1.2](../collation/README.md).
- **1c — first end-to-end (in-memory)** ✅ *landed*: the `COLLATE` postfix expression operator +
  `ORDER BY … COLLATE`, the resolver's collation derivation (`42P21` explicit-conflict, `42704`
  unloaded, `42804` non-text), `db.ImportCollation` into an **in-memory** database (no format change
  — placed in the committed catalog, not persisted; [api.md](api.md)), collated comparison, the
  `collate` cost unit, and the corpus `# load-collation: name = fixture` directive that drives it
  deterministically (`suites/collation/collate.test`, §10). The "it's alive" milestone for
  collation. Three refinements made here, all to match PostgreSQL / the cost contract: (a) **COLLATE
  binds at the postfix / typecast rung** (tighter than `||` and comparisons — PG; the §1 draft's
  "looser than `||`" was wrong); (b) the explicit-vs-explicit conflict is **`42P21`** not `42P22`
  (PG distinguishes them — §1; `42P22`, the *implicit* conflict, waits for per-column collations at
  1d); (c) the **`collate` cost is charged at comparison evaluation**, not in the (always-unmetered)
  ORDER BY sort (§11). A collated `ORDER BY` cannot use the `C`-ordered streaming/spill sorter, so
  it materializes + sorts via a decorate sorter (each sort key built once); collation is in-memory
  only, so it never spills (collated keys are slice 1e). The lexer gained a double-quoted-identifier
  token (`Token::QuotedIdent`) for collation names, consumed only in the COLLATE / ORDER BY position.
- **1d — on-disk baking** ✅ *landed*: `format_version` 17 — the `entry_kind` 3 baked collation
  snapshot (a flags byte `is_default`/`reference` + the LZ4-compressed `.coll` artifact, the artifact
  byte-identical to `db.SaveCollation` so a golden doubles as an artifact fixture) emitted *composites
  → sequences → collations → tables*; the per-column collation (the column flags byte gains bit 6
  `has_collation` + a trailing name); `db.ImportCollation` baked-persisting at `commit`,
  `db.ExportCollation`, `db.SetDefaultCollation`/`db.DefaultCollation`, `db.Collations` introspection;
  per-column `COLLATE "name"` in `CREATE TABLE` (text-only `42804`, loaded-name `42704`); un-annotated
  text columns inherit the per-database default, **frozen at CREATE TABLE** (PG-matching); the
  collation `collation_table.jed` golden (`rust == go == ts == ruby`). Refinements made here, all
  recorded below: (a) the **per-database default rides the `is_default` flag on its snapshot**, not a
  separate header/meta field — jed's catalog packs whole kind-tagged entries (no free-form header
  stream) and the meta page is fixed-width + CRC-protected, so a flag bit on the (already-present, since
  a non-`C` default must be loaded) snapshot is the clean home; `C` default ⇒ no snapshot flagged (§5).
  (b) **`42P22` (indeterminate_collation) is now reachable** — a column reference's *implicit*
  collation is its frozen collation (`C` counts as a distinct implicit collation, PG-matching), and two
  different implicit collations in one comparison / ORDER BY without an explicit `COLLATE` raise
  `42P22`; an explicit `COLLATE` on either side resolves it. The conflict is derived for **all**
  comparison ops including `=`/`<>` (PG raises it regardless), even though `=`/`<>` ignore the
  collation at eval (byte equality, §7). (c) Collation **derivation propagates** through a column
  reference (implicit), `COLLATE` (explicit), and `||` (combine); every other shape resets to none
  (takes a neighbour's) — the same documented narrowing as 1c. Set-operation output columns do not
  yet propagate an implicit collation (an explicit `COLLATE` on a set-op ORDER BY key still works) — a
  deferred follow-on.
- **1e — collated keys** ✅ *landed*: the sort-key key encoding as a new
  [encoding.md §2.12](encoding.md) sub-section (`text-collated-sortkey`), a collated text
  `PRIMARY KEY` / ordered secondary index / `UNIQUE` whose stored key is the column collation's UCA
  sort key (so the B-tree iterates in collation order with no runtime comparator), in all three cores
  + the Ruby reference. The key encoders thread a per-column resolved-collation slice; a non-`C` text
  key component encodes via `sort_key` (which already appends the §2.4 C-key, so it is self-delimiting,
  total, and reversible) instead of `text-terminated-escape`. No `format_version` change (the collated
  snapshot/per-column collation landed in 1d; 1e only changes how a *key* is computed). Verified by the
  `collation_pk_table.jed` golden (`rust == go == ts == ruby`, the key bytes pinned by
  [../collation/vectors/sortkey.toml](../collation/vectors/sortkey.toml)) + corpus
  (`suites/collation/collate.test`). Two refinements/narrowings, both recorded in §8: (a) **point-lookup
  pushdown is deferred for a collated key** — a collated PK/index `WHERE` full-scans + residual-filters
  (the planner already excludes a *collated* comparison from a byte-range bound, so a `C` text key still
  pushes down and a non-`C` one does not); (b) **one uniform component codec** — the full sort key
  (identical level included) is used in every key position (PK, index entry, `UNIQUE` prefix), the
  secondary-index `sort_key ‖ pk`-without-identical-level alternative not taken. An FK over a collated
  parent key encodes the probe with the **parent's** collation. The dev-collation unmapped-code-point
  case aborts a collated INSERT with `0A000`, the same code/point the comparison path raises.

> **Note — slices 1c/1d/1e above landed under the earlier baked/host-extracted model.** Their
> *algorithm, encoding, and SQL surface* stand (the §"Status" Retained list); their *persistence and
> host-load* path (`db.ImportCollation` baking, the format-17 baked snapshot) is **superseded by the
> reference-only pivot below** and is removed at that slice. The entries are kept as the build record.

**Slice 2 — the reference-only / vendored-tier pivot** (this revision; **in progress**), in
dependency order, and landing with or behind the [compatibility.md](compatibility.md) manifest it
leans on:

- **2a — vendoring source + sync** ✅ *landed (dev set)*: `gen_collation_vectors` also writes the
  `.coll` artifacts the cores embed (`spec/collation/fixtures/*.coll`); `scripts/vendor_collations.rb`
  distributes them per core (Rust `include_bytes!`es spec/ directly; Go gets raw copies +
  `//go:embed`; the browser-safe TS core gets a generated base64 module), with a `rake verify` drift
  gate. **Still pending:** moving `ExtractHostCollation`/`CompileCollation` to a build/tools target
  compiled *out of* production (§4.1), and the real version-pinned DUCET + curated non-CJK tailorings
  (`en-US`, `de`, `fr`, `es`, `sv`, `da`) replacing the dev fixtures (the §9 pipeline proper).
- **2b — vendored read path** ✅ *landed (all three cores)*: each core embeds the vendored `.coll`
  and resolves a collation by name from it (`resolveCollation`: referenced set, then vendored), so a
  collation is usable with **no import** — the database references it by name and the table comes from
  the binary. The corpus `# load-collation:` directive now resolves the dev collations via the
  vendored path (no import, nothing baked), proving the vendored bytes order identically cross-core.
  **Still pending:** the three build tiers (`C`-only / non-CJK / everything, §13) gating which `.coll`
  embed, and removing `db.ImportCollation`/`ExportCollation`/`LoadHostCollation` from production
  (keeping `db.SetDefaultCollation`/`DefaultCollation`/`Collations`).
- **2c — reference-only on disk** ✅ *landed (all three cores + Ruby)*: `format_version` 17 → **18**
  shrinks the `entry_kind` 3 entry to **metadata only** — a flags byte (`is_default`) + name +
  `(unicode_version, cldr_version)` pin + description; the LZ4-compressed baked table is gone. On open
  the table is resolved from the **vendored** set by name; a name this build does not vendor fails
  legibly (`42704`, the precursor to 2d's graded verdict). All 46 `.jed` goldens regenerated;
  `rust == go == ts == ruby` byte-identical. **Still pending:** the optional `.coll` content hash as
  open-time integrity defense.
- **2d — the graded verdict for collation** (§3/§12): wire collation into the
  [compatibility.md](compatibility.md) manifest + open-time verdict (full / read-only heap-scan /
  legible refusal) and the `REINDEX`/`COLLATION UPGRADE` migration. Requires `XX002` registered
  ([compatibility.md §7](compatibility.md)).

**Possible later follow-ons** — **none scheduled or committed**; recorded as candidate
directions the machinery leaves open, *not* a roadmap or a TODO list. Each would be its
own slice if and when there is a reason to pursue it:

- **`LIKE` / pattern matching under a non-`C` collation** (§7) — lift the byte-semantics
  narrowing.
- **CLDR `shifted` variable weighting** per locale (§6) — refine away `non-ignorable`, pinned
  to the oracle.
- **Nondeterministic collations** (case/accent-insensitive *equality*, §6) — the big one:
  forces the UNIQUE-collision / DISTINCT / GROUP BY / hashing / pattern paths to be handled.
- **CJK (Han) collation** (§13 tier-3 outlier) — authoring the multi-MB tailoring data; once
  vendored it is the **tier-3** build, a per-binary footprint choice (§13), not a per-file cost.

Because collations are vendored, not loaded, there is **no runtime loading surface to design** — no
`CREATE COLLATION … FROM HOST | FROM DEFINITION`, no host-API import. The only collation surface in a
production build *references* an already-vendored collation by name (§1); producing the vendored data
is the build-time pipeline (§9).

## 15. Divergences from PostgreSQL

Recorded per CLAUDE.md §1:

- **Default column collation is the per-database default recorded in the file** (itself `C` at
  creation, settable, §1) — **not** the host `LC_COLLATE` and **not** a hard-wired constant.
  (Reason: determinism + no ambient-locale dependency, CLAUDE.md §8/§10.)
- **Collations are vendored into the binary, not loaded from the OS** (§2/§9); PG resolves
  collations from the OS/locale environment at runtime. jed reads the host **only at build time** to
  *produce* the vendored data; the running engine has no collation host seam. (Reason: cross-core
  determinism — three cores' host ICU disagree on day one, §2 — plus keeping every runtime use pure.)
- **jed vendors its own compiled collation tables** at an embedder-chosen footprint tier (§13); PG
  links the host ICU/glibc. (Reason: a deterministic, growable, version-pinned set jed owns.)
- **The database file *references* its collations by name + `(unicode, cldr)` version; it never
  stores the table** (§3/§5). This *looks* like PG's `collversion` posture but is the opposite in
  substance: PG references the **host OS's drifting** library (silent-corruption risk); jed
  references **vendored, pinned, version-stamped** data and catches any skew with a graded open-time
  verdict (§12, [compatibility.md](compatibility.md)) — so jed still has **no
  collation-corruption-on-upgrade failure mode**, the central divergence. The cost is that a Unicode
  bump makes an old file *read-only until `REINDEX`* on the new binary (§12), where PG's baked-nothing
  model and jed's old baked model each avoided that in their own way.
- **Collated indexes store UCA sort keys** (memcmp-ordered, §8); PG stores the original and
  compares with a runtime comparator. (Reason: the single-`memcmp`-order storage contract,
  [encoding.md §1](encoding.md).)
- **Only deterministic collations in the first slice** (§6/§14); PG ships nondeterministic ICU
  collations from the start.
- **No `CREATE COLLATION` / runtime loading surface at all** (§14); a collation is vendored at build
  time or it does not exist for that binary.

Where jed *does* vendor a collation, its **ordering matches PostgreSQL's same-locale ICU ordering**
for the supported levels (the conformance default, §10) — the divergences above are about *which*
collations exist, *where* their data lives, *how* it is delivered, and *how* keys are stored, not
about getting a supported locale's order wrong.
