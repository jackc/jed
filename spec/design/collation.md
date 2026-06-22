# Collation — design

> Linguistic (locale-aware) collation for `text`: dictionary-style ordering (`ä` near `a`,
> not after `z`) layered on the existing UTF-8 `text` type via a `COLLATE` clause, a per-column
> collation, and a **per-database default collation** recorded in the file. The engine **owns the
> collation algorithm** (a hand-written Unicode Collation Algorithm, UTS #10, in every core) and
> **loads the compiled collation tables from a host-supplied *Unicode-data bundle*** (§13) — the
> bare binary is **pure `C` / ASCII** and carries **no** Unicode tables; a host hands the engine
> bundle **bytes** (from a file, a fetch, or a compiled-in asset — the engine never does the I/O)
> and the collations in it become usable. The database file **never snapshots collation data**: it
> **references** the collations it uses by name and records the one
> **`(unicode_version, cldr_version)`** its keys were built under (§5); version skew between that
> pin and the loaded bundle is resolved by the **graded, legible open-time verdict** of
> [compatibility.md](compatibility.md) (full / read-only heap-scan / legible refusal), not by
> carrying tables in the file. Bundles are produced by a **build-time pipeline + builder tool** (raw
> DUCET/CLDR → canonical jed definitions → compiled `.coll` artifacts → a `JUCD` bundle, §9);
> `ExtractHostCollation` and `CompileCollation` are **build-time tooling, compiled out of the
> production engine** (§4/§9). The **production** collation surface is therefore small: read a
> loaded table (`OpenCollation`) and run the executor; the SQL surface only ever *references* a
> loaded collation by name (§1). This doc is the contract all three
> cores implement in lockstep (CLAUDE.md §2). The `text` type and the `C`-collation baseline are in
> [types.md §11](types.md); the key-encoding rule in [encoding.md §2.4](encoding.md); the
> catalog/byte layout in [../fileformat/format.md](../fileformat/format.md); the LZ4 codec that
> compresses the `.coll` artifact's table in [large-values.md](large-values.md); the host-seam pattern in
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
> **Status update — Slice 3 (host-loaded Unicode-data bundle; THIS revision, proposed).** The
> vendored-tier model above is **further revised**: collation tables are no longer compiled into
> the binary at a build-time *tier*. The bare binary is **pure `C` / ASCII** and a host **loads** a
> **`JUCD` Unicode-data bundle** — one shared DUCET root + per-locale tailoring **deltas** + the
> Unicode **property/casing** tables (§9/§13) — by handing the engine **bytes** through a
> privileged, **bytes/reader-based** host call (§4), never a file path, so the engine still does no
> I/O (the BlockStore principle, [hosts.md](hosts.md)). A self-contained binary is just the *host*
> sourcing those bytes from a compiled-in asset (`include_bytes!` / `//go:embed` / a bundled TS
> asset); there is **no build-time embed *mode* in the engine.** This is a **packaging/delivery**
> change, **not** a semantics change: the `.coll` table, the UCA compiler/executor, the sort-key key
> encoding, and the metadata-only file reference entry (§5) are all **retained** — only *where the
> table bytes come from* moves from `include_bytes!` to a host load, so cross-core byte-identity
> (§8) is unaffected (the same pinned bytes whatever their source). The footprint tiers (§13) become
> **builder-tool bundle presets** (`casing-only` / non-CJK / everything), and **casing follows
> collation out of the binary**: the bare binary's `upper`/`lower` fold **ASCII only** (passing
> non-ASCII through, the SQLite default), full Unicode casing engaging only when a bundle's property
> section is loaded — so a `C`/ASCII-only database carries **zero Unicode-version surface** (§16).
> Built as **Slice 3** (§14), in lockstep across the three cores, after this spec lands.
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
> - **Landed since (slice 1f):** the **real version-pinned DUCET root** — `unicode`, the CLDR-tailored
>   DUCET (UCA/UCD **17.0.0**, CLDR 48, `spec/collation/17.0.0/root.allkeys`, the table ICU/PostgreSQL
>   use) — and `es` (root + the Spanish `&N<ñ<<<Ñ` tailoring) replace the `dev-*` fixtures in the
>   production set (now shipped as the host-loaded `unicode.jucd`, §9), in all three cores + the Ruby
>   reference (byte-identical `.coll`,
>   oracle-clean against `postgres:18`'s ICU for the covered letters). The `dev-*` fixtures are
>   retained **only** as the small cross-core compiler/sort-key vectors. CJK/implicit-weight ranges
>   (tier-3) raise `0A000`; the embedder-chosen footprint tiers (§13) and the broader tailoring set
>   (sv/da/de — needing the deferred LDML `[before]`/expansion/contraction features) remain follow-ons.
> - **Landed (Slice 3 — host-loaded `JUCD` bundle):** the **bundle codec** (`JUCD`, §9 /
>   [../collation/README.md §5](../collation/README.md)) with **root-sharing** (the DUCET root once +
>   per-locale sparse deltas merged at load, byte-identical to the full `.coll` — the merge-identity
>   vectors) in all three cores; the **bytes/reader load seam** `db.LoadUnicodeData` + introspection
>   `db.LoadedCollations` (§4.2); the **builder tool** `build_collation_bundle` (3b — `--preset`
>   non-CJK/everything/casing-only + `--out`, the preset-driven assembler that produces the shippable
>   bundle, §4.1); and the real production bundle `spec/collation/fixtures/unicode.jucd` (root `unicode`
>   + `es`) that the cores LOAD. The compile-time embed (`include_bytes!` / `//go:embed` / base64) is
>   **gone** — the bare binary carries no Unicode data (the SQLite model, §16); embedding is now a host
>   choice (the host hands the same bytes to `LoadUnicodeData`).
> - **Landed (Slice 3e — casing):** the **ASCII-casing baseline + property section** — `upper(text)`/
>   `lower(text)` (the text overload of the range accessors) and `ILIKE` in all three cores; the
>   property/casing section is populated (UCD 17.0.0 case mappings, [../collation/17.0.0/casing.txt](../collation/17.0.0/casing.txt))
>   and rides `unicode.jucd` + the `casing-only` preset (§16/§13). The bare binary folds ASCII only.
> - **Landed (Slice 2d — the graded version-skew verdict):** collation is the **first implemented
>   instance** of [compatibility.md](compatibility.md)'s graded open-time verdict (§12/§14). `XX002`
>   (`collation_version_mismatch`) is registered; on open, each referenced collation's file-pinned
>   `(unicode, cldr)` is compared to the loaded bundle's — **same version** reads-writes normally,
>   **different version** leaves the object **read-only** (reads recompute against the loaded table,
>   the heap-scan fallback that is jed's *existing* collated execution path; a write that would mix two
>   orderings in one B-tree is refused `XX002`), and an **entirely absent** referenced collation
>   **refuses the open** `XX002` (was `42704`). No `format_version` bump — the verdict is a pure
>   comparison of data already in the file (§5) + the loaded set. The `REINDEX`/`COLLATION UPGRADE`
>   migration that clears a skew is a **follow-on**.
> - **Not yet built (Slice 3 remainder):** `initcap` (word-boundary titlecasing) + `normalize`/regex
>   (deferred property sub-tables); implicit weights / the full CJK tier-3 root; the broader LDML
>   tailoring features (and the sv/da/de tailorings that need them); the `REINDEX`/`COLLATION UPGRADE`
>   migration (the 2d follow-on that rebuilds + re-pins a skewed table). (The slice-2 "embedder-chosen
>   footprint tiers" are **superseded** by Slice 3's builder-tool bundle presets — §13.)
>
> Two foundational choices are unchanged: the definition format is the **UCA/CLDR standards** (DUCET
> `allkeys.txt` + LDML), and the `.coll` **compiled artifact is the one shared cross-core form** every
> core loads (via the `JUCD` bundle) and reads (`OpenCollation`) — never a per-core compile (§9, the
> [timezones.md §3.3](timezones.md) compiled-TZif precedent).

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

A collation is **provided by a loaded Unicode-data bundle** (§9/§13) — the bare binary carries none.
A collation name is usable in a database iff a loaded bundle provides it; naming one no loaded bundle
provides is **`42704`** (`undefined_object`), the same code an unknown array/range element type
raises. Loading is a **privileged host operation** (`db.LoadUnicodeData`, below) that hands the
engine bundle **bytes** — *not* SQL-reachable and *not* a filesystem path, so an untrusted query can
only ever *use* an already-loaded collation by name, never trigger a load, and the engine itself does
no I/O (§11). This is categorically distinct from the host-`LC_COLLATE`/ICU **runtime read** the
architecture forbids (§2): the loaded bytes are jed's **own pinned `.coll`** tables, byte-identical
whatever their source (file, fetch, or compiled-in asset), so loading restores **no nondeterminism**
to the engine — every *use* stays pure. (This supersedes both the slice-1 explicit host-import
lifecycle and the slice-2 build-time *vendoring*: there is no host seam in the running engine, only a
privileged load of jed's own pinned bytes.)

```
// production host API (privileged — not untrusted SQL, §11):

db.LoadUnicodeData(bytesOrReader)  // load a JUCD bundle: its collations + Unicode property tables (§4/§9); additive
db.SetDefaultCollation("en-US")    // set the per-database default (must be provided by a loaded bundle)
db.DefaultCollation()              // read the per-database default
db.Collations()                    // introspect THIS database's referenced collations: name, (unicode, cldr), description
db.LoadedCollations()              // introspect the loaded set available to any database on this handle

// build-time tooling ONLY — compiled out of the production engine (§4/§9):
//   ExtractHostCollation(name)        host ICU/CLDR  → canonical jed definition
//   CompileCollation(name, defReader) definition     → compiled `.coll` table
//   SaveCollation / OpenCollation     `.coll` artifact serialize / deserialize
//   (builder tool)                    selected `.coll` tables → a JUCD bundle (shared root + deltas, §9)
// These produce the shippable bundle; production LOADS one via db.LoadUnicodeData and reads its
// tables via OpenCollation, and never compiles or reaches the host.
```

```sql
-- SQL surface (a collation just needs to be provided by a loaded bundle):
CREATE TABLE people (id i32 PRIMARY KEY, name text COLLATE "en-US")
CREATE INDEX ON people (name)                   -- ordered by the column's en-US collation
SELECT name FROM people ORDER BY name             -- en-US order
SELECT name FROM people ORDER BY name COLLATE "C" -- override: byte order
SELECT 'ä' < 'z' COLLATE "de"                      -- per-expression collation (de must be in a loaded bundle)
```

- **`COLLATE "name"`** is a postfix operator on a text expression yielding the same value with
  a different collation for the surrounding comparison/sort. It binds at the **postfix / typecast
  level** — the same rung as `::` / `[]` / `.field`, so **tighter than `||` and the comparison
  operators** ([grammar.md §47](grammar.md), PostgreSQL precedence: `a || b COLLATE "x"` is
  `a || (b COLLATE "x")`, `'a' < 'b' COLLATE "x"` is `'a' < ('b' COLLATE "x")`). Naming a
  collation **not provided by a loaded bundle** is **`42704`** (`undefined_object`), the same code
  arrays/ranges raise for an unknown element type; applying `COLLATE` to a non-collatable
  (non-text) expression is **`42804`** (`datatype_mismatch`, PG-matching). The name is a quoted
  identifier — a non-`C` type is a 1c-only narrowing on which the version-pinned real collation
  set later builds. *(Slice 1c implements COLLATE at the postfix rung; an earlier draft of this
  doc said "looser than `||`", which mis-stated PG — corrected here.)*
- **Collation names are quoted identifiers** (they contain hyphens): `"C"`, `"en-US"`, `"de"`,
  `"sv"`. `"C"` is always available; every other name must be provided by a **loaded bundle** (§13).
- **Per-database default collation** (§3). Every database has a default collation **recorded in
  its file** (by name + version, never as data); an un-annotated `text` column uses it. It is
  **`C` at creation** and can be set to any *loaded* collation via `db.SetDefaultCollation`.
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
- **Provenance + introspection.** Each collation carries an optional, human-readable
  **description** recording where it came from — auto-filled at **build time** by
  `ExtractHostCollation` with the core/OS/library identity (e.g. `Go 1.26.3 / Linux 7.1 / ICU 73`),
  baked into the `.coll` artifact (§4), travelling with the table in the bundle, and surfaced by
  introspection (`db.Collations()` / `db.LoadedCollations()` → name, `(unicode, cldr)` version,
  description). It is descriptive metadata only — **excluded from the content hash** (§4), so it
  never affects ordering or dedup.

## 2. The fixed architecture: jed owns the algorithm; tables are loaded, not host-read

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

So jed **owns the compiled collation tables** (§9/§13) and the running engine reads them from a
**host-loaded bundle** — the host environment is consulted **only at build time**, to *produce* the
bundle, never by the running engine. (Two earlier models are superseded: one vendored nothing and
read the host at runtime via an explicit load — a non-deterministic seam in the engine; the other
compiled the tables into the binary at a build *tier*. The first left a host seam in the engine; the
second couples footprint to the build. Loading jed's **own pinned bytes** from a bundle has neither
problem — the bytes are deterministic, and footprint becomes the deployer's choice, §13.) The
architecture has three layers; **only the lower two ship in the production engine**, and they are the
cross-core contract:

1. **The jed collation table** — jed's own compiled, executable form (collation elements +
   multi-level weights, §6), loaded from a bundle and read at startup. What the executor runs on.
2. **The executor** (table → ordering / sort key, §6) — **jed-owned, hand-written per core,
   spec'd** (CLAUDE.md §5 forbids codegenning it), **cross-core byte-identical given identical
   input**, verified by byte fixtures (§10), exactly the composite/array precedent
   ([extensibility.md §4.1](extensibility.md)). This is the **whole production collation code**:
   load a `JUCD` bundle (§9), deserialize each `.coll` (`OpenCollation`, after the root + delta
   merge §9), and run the executor.
3. **The build-time pipeline + builder tool** (§9) — the **compiler** (`CompileCollation`: canonical
   definition → jed table), **the host seam** (`ExtractHostCollation`: raw host ICU/CLDR →
   definition), and **the builder** (selected `.coll` tables → a `JUCD` bundle). These *produce* the
   shippable bundle and are **compiled out of the production engine.** The compiler stays
   hand-written + cross-core-tested (its vectors, §10) so any core's build can regenerate the pinned
   `.coll` byte-identically — but no core invokes it at runtime.

> **The determinism boundary, stated once:** cross-core byte-identity is a property of *a jed table +
> the executor*. The table is **the same `.coll` bytes whatever their source** (§9) — loaded from a
> bundle the host supplies (a file, a fetch, or a compiled-in asset), all byte-identical — so a query
> never observes any host variation; it runs over identical loaded bytes. All the messy host reading
> still happens **once, at build time** (`ExtractHostCollation` → `CompileCollation` → the committed
> `.coll` → the `JUCD` bundle), behind a CI reproducibility check (§9/§10); **loading** the resulting
> bundle restores no nondeterminism. This is the same shape as the storage seam (fixed behavior over
> host-supplied bytes), not the clock seam (a per-query draw) — the host supplies bytes once, the
> engine consumes them repeatably.

## 3. Where the data lives: loaded from a bundle, referenced in the file

The collation table is **loaded from a host-supplied bundle** (§9/§13), and the database file
**references it** — it never carries the table:

- **Loaded (the only mode).** The compiled jed table reaches the engine in a `JUCD` bundle the host
  hands it (§9); the host may source those bytes from a file, a fetch, or a compiled-in asset (§13).
  The file records only **which collations it uses** (by name) and the one
  **`(unicode_version, cldr_version)`** its keys were built under (§5). Storing the table per-file
  would not shrink the distribution — it would only add a second copy and a cross-version-skew hazard
  (a file accumulating collations from different versions). So jed does not.
- **Skew is handled by the verdict, not the file.** A file pinned to `(unicode, cldr) = X` opened
  with a loaded bundle also at `X` → full read-write. With a bundle at a *different* version (or
  none providing the collation) → the **graded open-time verdict** of
  [compatibility.md](compatibility.md): **read-only heap-scan** (values are version-independent,
  [compatibility.md §4.1](compatibility.md); the suspect collated index is not used for
  acceleration and not maintained until a migration rebuilds it) or, for an entirely absent
  read-required dependency, a **legible refusal** naming the missing collation + version. This
  *replaces* the old baked-vs-reference choice and its host-reimport hash check.

Crucially, this is **not** PostgreSQL's host-dependent posture: the reference is to **loaded,
pinned, version-stamped** jed data that moves only on a discrete jed release — not to the host OS's
drifting ICU/glibc. A file is fully portable to **any binary with a loaded bundle of the same
Unicode version**, and degrades *legibly* (never silently-wrong rows) elsewhere. A database that
uses only `C` (the creation default) carries **zero** collation data, needs **zero** loaded tables,
and (with only ASCII casing, §16) pins **no Unicode version at all** — portable to every jed binary,
forever.

Every collated index records the `(name, unicode_version, cldr_version)` it was built under (the
stamp). It is what the open-time verdict checks against the loaded bundle's version and what makes
a deliberate re-collation (§12) a *controlled* event.

## 4. The build-time pipeline and the production surface

The lifecycle splits cleanly in two: a **build-time pipeline + builder tool** that produce the
shippable **`JUCD` bundle**, and a **production surface** that **loads** a bundle and only ever
*references* its collations by name. A **`Collation`** is the unit the pipeline manipulates — a jed
table (§6) plus its metadata (`name`, `(unicode, cldr)` version, content `hash`, optional
`description`).

### 4.1 Build-time pipeline (compiled out of the production engine)

These run when the shippable `spec/collation/` data is **regenerated** — typically only on a Unicode
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
  shared cross-core form**: the committed `spec/collation/` fixtures (§9) and the bytes a host loads
  are the same `.coll`. **`OpenCollation` is the one pipeline routine that also ships in production**
  (§4.2, the read path); `SaveCollation` and the producers above are build-time only.
- **The builder tool — `.coll` set → `JUCD` bundle (build-time).** Assembles selected compiled
  `.coll` tables into the shippable **`JUCD` bundle** (§9): a shared DUCET **root** section, per-locale
  tailoring **deltas** against it, and the Unicode **property/casing** section, with presets
  (`casing-only` / non-CJK / everything, §13). Deterministic; its bundle bytes are a §10 byte fixture.
  Landed as `impl/rust/src/bin/build_collation_bundle.rs` (`--preset` / `--out`; reads the committed
  `.coll` set, self-checks the merge identity, writes the bundle) — Rust-only build-time tooling, like
  the compiler `gen_collation_vectors` (the other cores only *load* the bundle, §4.2). It writes the
  canonical `spec/collation/fixtures/unicode.jucd` at `non-cjk`; `casing-only` awaits the property
  section (§16, slice 3e).

### 4.2 Production surface

The shipped engine carries **`OpenCollation` + the root+delta merge (§9) + the executor only** (§2
layer 2). The host supplies a bundle's bytes; the engine reads its `.coll` set into in-memory tables;
thereafter:

- **`db.LoadUnicodeData(bytesOrReader)`** — privileged host op (not SQL-reachable, no path, no I/O in
  the engine, §11): parse a `JUCD` bundle, merge root + deltas (§9), and add its collations + property
  tables to the loaded set. **Additive** — multiple bundles may be loaded; resolution is by name in
  load order (a name an earlier bundle already provides is kept, §9). The host sources the bytes (file
  / fetch / compiled-in asset), which is the whole of the "self-contained binary vs. external file"
  choice — there is no engine-side mode. The loaded set is **engine-global** (a property of the running
  engine, not of one handle — "the loaded set available to any database on this handle"), which is what
  lets a host load a bundle **before opening** a file that *references* one of its collations: open
  resolves the referenced table from the loaded set (§5), and `open` mints the handle, so the data
  cannot live on the handle. Each core therefore exposes the load as both the `db.` method here and an
  engine-level call the host may invoke prior to `open` (Rust `jed::load_unicode_data`, Go
  `jed.LoadUnicodeData`, TS `loadUnicodeData`); both populate the one engine-global set.
- **`db.SetDefaultCollation(name)` / `db.DefaultCollation()`** set/read the per-database default
  (the name must be in a loaded bundle, else `42704`).
- **`db.Collations()`** introspects **this database's referenced** collations; **`db.LoadedCollations()`**
  introspects the loaded set available on the handle: `name`, `(unicode, cldr)` version, `description`.
- the SQL surface (`COLLATE`, per-column collation, `ORDER BY … COLLATE`) **references** loaded
  collations by name.

There is still **no host-ICU import path** — `db.ImportCollation` / `ExportCollation` /
`LoadHostCollation` do not exist; the only load is of jed's **own pinned bundle** via
`db.LoadUnicodeData`, which constructs no table (it deserializes pinned bytes) and reaches no host
data. **`C` is never bundled, loaded, or referenced** — it is table-free and built in.

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

  It carries **no compiled table and no LZ4 blob** — the table is **loaded from a bundle** (§2/§9).
  (An optional content **hash** of the `.coll` may be recorded as a cheap open-time integrity check
  against a mis-built bundle; it is *not* load-bearing for correctness the way the old host-reimport
  hash was, since `(name, unicode, cldr)` already uniquely identifies the committed `.coll`.) This
  metadata-only entry already shipped at **`format_version` 18** (slice 2c, which removed the
  `format_version` 17 baked snapshot); **Slice 3 changes only *delivery* — how the table reaches the
  engine — so it does *not* bump `format_version`, and the on-disk entry, the goldens, and the
  collated key bytes are byte-for-byte unchanged.**

The per-column collation rides the slot [format.md](../fileformat/format.md) already reserves
for it (the per-column flags + typmod-adjacent field, where `varchar(n)` and the composite/array
type descriptors live). An **index entry** records the collation it was built under by
`(name, unicode_version, cldr_version)`.

The on-disk bytes are version-independent of any table: every core with a **loaded bundle at the same
`(unicode, cldr)`** computes identical sort keys (§8) → a byte-identical collated B-tree in the
goldens (§10). A core with a bundle at a *different* version (or none providing the collation) does
not silently produce a divergent tree — it hits the open-time verdict (§3/§12,
[compatibility.md](compatibility.md)).

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
  **not-loaded collation** (`42704`), and **non-text COLLATE** (`42804`) are the new errors in this
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
The sort key is produced by the **loaded** table (§2/§9), so every core with a loaded bundle at the
same `(unicode, cldr)` version emits identical key bytes → byte-identical collated B-trees.

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

## 9. The data: the build-time pipeline, the `JUCD` bundle, and root-sharing

The pipeline turns raw Unicode/CLDR data into the **one shared `.coll` form**, which the builder tool
packs into a **`JUCD` bundle** the host loads. Everything before the load runs **at build time** —
none of it ships in the production engine (§4.1):

```
raw Unicode data:  DUCET allkeys.txt + CLDR LDML tailorings + UnicodeData/SpecialCasing  (pinned: unicode_ver, cldr_ver)
        │   ExtractHostCollation / a normalizer   (build-time tooling — host-dependent)
        ▼
canonical jed definitions   spec/collation/<ver>/*.allkeys + *.ldml + casing source   (committed source, auditable)
        │   CompileCollation  (run ONCE — cross-core-deterministic, §6)
        ▼
compiled artifacts          spec/collation/<ver>/*.coll                              (committed, byte-pinned golden)
        │   the BUILDER TOOL: shared root + per-locale deltas + property section → a JUCD bundle (presets §13)
        ▼
shippable bundle            *.jucd                                                    (committed / distributed; README §5)
        │   the HOST sources these bytes (file / fetch / include_bytes! / //go:embed / TS asset)
        │   db.LoadUnicodeData → merge root + deltas → OpenCollation  (production — the ONLY stage that ships)
        ▼
in-memory jed tables → the executor (§6)
```

The **`JUCD` bundle** is a manifest-indexed container (byte format in
[../collation/README.md §5](../collation/README.md)) holding three kinds of section: the shared DUCET
**root** (the ~0.3 MB bulk, stored **once**), per-locale tailoring **deltas** against it (a few KB
each), and the Unicode **property/casing** tables (§16). A loader takes only what it needs — a
casing-only host loads just the property section and never pays the root; a browser fetches the
manifest + root, then a locale's delta on demand.

Three properties make this safe, small, and cheap:

- **Compile once, load identical bytes.** The `.coll` and the bundle are produced by a single
  pipeline run and committed as byte-pinned goldens; every host loads the **same bytes** and the
  engine reads them with `OpenCollation`. Cross-core byte-identity is then **trivial** (same input
  bytes) rather than contingent on every core's compiler agreeing — exactly the
  [timezones.md §3.3](timezones.md) reasoning for vendoring compiled TZif rather than running `zic`
  per core. The compiler still ships cross-core vectors (§10) so **any** core's build can regenerate
  the pinned `.coll` byte-identically (no reference implementation, CLAUDE.md §2), behind a **CI
  reproducibility check** that re-runs the pipeline and diffs against the committed bytes.
- **Root-sharing via delta + load-time merge.** Because a tailoring resolves *into* a full table
  (README §2 — `es.coll` and the root differ by a handful of entries), a bundle stores the root once
  and each locale as a **sparse override** (the `single`/`contraction` entries it adds-or-replaces).
  `db.LoadUnicodeData` performs a deterministic, spec'd **merge** — start from the root maps, apply
  the delta by key, re-sort — producing a table **byte-identical to the fully-resolved `.coll`** the
  build produced for that locale. The executor (§6) is **unchanged**; only the load gains a merge
  step, and the merge is a §10 byte fixture (`merge(root, es-delta).table == es-full.table`). This is
  what makes a 10-locale bundle ~0.4 MB instead of ~3 MB.
- **The host is read only at build time.** `ExtractHostCollation`'s non-determinism is contained by
  pinning its output (the committed definition + `.coll` + bundle), never by trusting a per-run
  extraction. **Loading** the pinned bundle introduces no host data and no nondeterminism.

**`spec/collation/`** (a spec data directory parallel to `spec/encoding/`) holds the **byte-format
spec, fixtures, and verification vectors** — *repo data* — that double as the **source the bundle is
built from**. The byte formats are pinned in [../collation/README.md](../collation/README.md) (the
definition format §1, the compiled table §2, the `.coll` artifact §3, the sort key §4, the `JUCD`
bundle §5). It holds:

- the **definition format spec** (DUCET `allkeys.txt` subset + LDML tailoring subset) and the pinned
  `(unicode_version, cldr_version)` of the real root,
- the **definition fixtures** (the dev `dev-root.allkeys` + `dev-nordic.ldml`; the curated `en-US`,
  `de`, `fr`, `es`, `sv`, `da` set — the last two for the sharp `å ä ö`/`æ ø` after-`z` cases — as a
  follow-on), plus the **Unicode property/casing source** (§16),
- the **compiled `.coll` artifacts** those definitions produce — *both* the corpus's deterministic,
  host-free collation source *and* the bytes the builder packs into a bundle,
- the **`JUCD` bundle(s)** the builder emits (shared root + deltas + property section, §5/§13),
- **compiler vectors** — `(definition fixture) → (expected `.coll` / jed table bytes)`,
- **executor / sort-key vectors** — `(collation, string) → (sort-key bytes)`, the §8 byte-fixture
  pattern (CLAUDE.md §8) and the primary cross-core contract for the algorithm,
- **bundle vectors** — `(bundle bytes) → (manifest + per-section round-trip)` and the merge identity
  `merge(root, delta).table == full.table` (§10).

So both the corpus and the production cores obtain collations *deterministically* from the committed
`.coll` / bundle — never `ExtractHostCollation`, never a runtime compile, independent of any host.

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
  metadata-only entry (§5) and the collated B-tree's key bytes (produced by the **loaded** `.coll`)
  in one fixture. The `.coll` itself is pinned separately by the compiler vectors above. (The on-disk
  goldens are **unchanged** by Slice 3 — delivery moves, the stored bytes do not, §5.)
- **`JUCD` bundle + merge vectors** — the bundle round-trip (`Open`∘`Save` byte-exact on every core,
  [../collation/README.md §5](../collation/README.md)) and the root-sharing **merge identity**
  `merge(root, delta).table == full.table` (§9), so the load-time merge is a cross-core **byte
  contract**, not per-core code.
- **Conformance entries** drive collation by **referencing a loaded `.coll`** (the committed
  bundle / fixture, never `ExtractHostCollation`), so all three cores read the identical table →
  identical orderings; oracle-checked against `postgres:18` where jed matches PG and
  overridden-with-reason where it diverges (§15).
- **`ExtractHostCollation` (the build-time host seam) is tested per core**, against that core's own
  host — the [conformance.md](conformance.md)/CLAUDE.md §10 carve-out for "what the corpus cannot
  express" (host introspection / platform-specific behavior), since the host path is
  *deliberately* not cross-core-identical (§2/§4). It is a *tooling* test, not a production-engine one.

## 11. Untrusted-query safety, cost, and the determinism ledger

- **Loading is a privileged host op; using is pure** (CLAUDE.md §13). Slice 3 reintroduces a load
  path — but a *narrow, safe* one, categorically unlike the host-ICU read the architecture forbids
  (§2). `db.LoadUnicodeData` is a **privileged host-API call** taking pinned bundle **bytes** (or a
  reader): it is **not SQL-reachable** (an adversarial query cannot trigger a load), takes **no
  filesystem path** (the engine does no I/O — the host sources the bytes, [hosts.md](hosts.md)), and
  constructs no table from host data (it deserializes + merges jed's **own pinned bytes**, §9). So an
  untrusted query can only ever *use* an already-loaded collation by name, or get `42704`. Using a
  collation is **pure** — a string and a loaded table in, a sort key out; no host reach, no I/O, no
  nondeterminism. (`db.LoadUnicodeData` / `db.SetDefaultCollation` / introspection are privileged
  host-API ops, never on the untrusted surface.) The only thing that ever *read the host*
  (`ExtractHostCollation`) remains **build-time tooling, compiled out of the production engine**
  (§4.1).
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
- **Collation *use* stays OUT of the determinism ledger.** Because a query runs over a **loaded**
  table with a jed-owned executor, it is a deterministic function of its inputs — precisely the
  outcome [determinism.md §3](determinism.md) demands. Which collations are loaded is a
  host/configuration boundary (like *which file you opened*), not a query-time draw, so it needs no
  ledger entry either: no query observes the load (§2). (The ASCII-casing baseline §16 is likewise
  deterministic by construction, and full Unicode casing from a loaded property section is
  deterministic-given-the-bytes — so casing stays out of the ledger too.)

## 12. Migration and version adoption

> **Status: the graded verdict LANDED (Slice 2d, all three cores).** The three cases below are
> implemented as a pure on-demand comparison of the file's pin (§5) against the loaded bundle's
> version — `XX002` registered, no `format_version` bump (§14 2d for the full scope + the two
> deliberate narrowings: an *absent* referenced collation refuses the open rather than degrading
> per-object, and the `REINDEX`/`COLLATION UPGRADE` step is a follow-on, so a skewed table is
> read-only until it lands).

The reference-only model (§3) keeps a jed upgrade from *silently* breaking a file, while pinning +
the graded verdict make any genuine version move legible:

- **Same loaded version → opens fully.** A file pinned to `(unicode, cldr) = X` opened with a loaded
  bundle providing `X` reads-writes normally — collated structures use the loaded table, no re-sort.
- **Different loaded version, or no bundle providing it → graded verdict, never wrong rows.** A binary
  with a bundle at a *different* `(unicode, cldr)` (or no loaded bundle providing the collation) does
  **not** silently re-order: the open-time verdict ([compatibility.md §7–§8](compatibility.md))
  degrades the affected object to **read-only heap-scan** — values are version-independent
  ([compatibility.md §4.1](compatibility.md)), so the base table reads correctly; the suspect collated
  index is not used for acceleration and not maintained — or, for an entirely absent read-required
  dependency, **refuses legibly** naming the missing collation + version. The optional `.coll` hash
  (§5) catches a *mis-built* bundle that carries wrong bytes under the right version label.
- **Adopting a newer Unicode/CLDR version is explicit and opt-in.** Loading a bundle built on the new
  version + a `REINDEX` (or an `ALTER … COLLATION UPGRADE`-style op, named at the slice) rebuilds the
  affected indexes against the newly-loaded table and re-pins the stamp. The user chooses when to pay
  the re-sort; nothing forces it. (This is the concrete cost reference-only adds over the old
  bake-forever model: after a jed Unicode bump an old file is **read-only until REINDEX** on the new
  binary, rather than fully usable forever — accepted because the data stays readable, the
  degradation is legible, and collation versions move rarely.)

This is still a sharp contrast with PostgreSQL: PG depends on the **host OS's** ICU/glibc, which
drifts *silently* under an OS upgrade and may corrupt an index with only a `collversion` warning.
jed's reference is to **loaded, pinned, version-stamped** jed data that moves only on a discrete jed
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

## 13. Sizes — bundle presets, not build tiers

The footprint is a **deployment choice**, not a build/link choice and not a per-file cost (§3). The
bare binary carries **zero** Unicode data; a host loads exactly the bundle it needs. The slice-2
notion of three *build tiers* is **superseded** — the same coverage points survive as **builder
presets** (§4/§9), each just a selection of sections packed into a `JUCD` bundle, choosable when the
bundle is produced and swappable **without rebuilding the engine**.

| Preset (bundle contents) | Sections | Size (LZ4) |
|---|---|---|
| **bare binary** (no bundle) | none — `C` collation + ASCII casing are built in (§16) | **0 bytes**, **no Unicode version** |
| **`casing-only`** | property/casing section only | **tens of KB** |
| **non-CJK** (the common bundle) | property + shared root + all non-CJK tailorings | **< ~1 MB** (root ~0.3–0.5 MB **shared once** + a few KB per locale + casing) |
| **everything** | non-CJK + the CJK (Han) tailoring | non-CJK + low **single-digit MB** (the one outlier) |
| *(in file, any preset)* | none — name + `(unicode, cldr)` + optional description/hash | **tens of bytes** |
| *(for contrast) full ICU `.dat`* | never shipped — we own our surface | ~30 MB |

Notes that shape the presets:

- **Root-sharing is what shrinks the multi-locale bundle.** A non-CJK bundle stores the ~0.3 MB DUCET
  root **once** and each locale as a small delta (§9), so it is **< ~1 MB**, not the ~2–3 MB a
  per-collation-full-table packing would cost. The bundle's manifest lets a loader take only what it
  needs (a browser loads the manifest + root, then a locale's delta on demand).
- **Casing rides the same bundle, gated separately.** The universal Unicode **property tables** for
  `lower`/`upper`/`normalize`/regex are the bundle's **property section** (§16), on the **same one
  `(unicode_version)`** axis as the collation root — so a single version-stamped bundle keeps casing
  and collation from ever mismatching. A `casing-only` host loads just that section (no root); the
  bare binary loads neither and still has working ASCII `lower`/`upper` (§16).
- **The file's cost is flat.** A `C`-only / ASCII-only database carries zero Unicode data and pins no
  version (§3); any other database carries only **reference metadata** (tens of bytes per distinct
  collation), never a table.
- **The web/OPFS target benefits most** — a browser ships the *bare* engine and `fetch`es a bundle (or
  just its casing section) on demand, instead of base64-bundling megabytes of collation into the
  worker JS. The preset maps onto the existing capability-tier system (CLAUDE.md §7).

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
  compiled *out of* production (§4.1).
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
- **2d — the graded verdict for collation** ✅ *landed (all three cores)*: collation is the **first
  implemented instance** of [compatibility.md](compatibility.md)'s graded open-time verdict. `XX002`
  (`collation_version_mismatch`) is registered ([compatibility.md §7](compatibility.md), the
  CLAUDE.md §13 "fails closed and discoverably" code). The verdict is a **pure on-demand comparison**
  — the file already pins `(unicode_version, cldr_version)` per referenced collation (§5) and the
  loaded bundle carries its own, so no new stored state and no `format_version` bump. Per referenced
  collation: **`Full`** (loaded bundle provides the name at the **same** version → behaves exactly as
  before) / **`Skewed`** (name present, **different** version). Enforcement, per object = per table:
  - **Reads always work** — jed already executes *every* collated operation by heap-scan + in-memory
    recompute against the **loaded** table (collated keys never push down — the 1e narrowing §8; a
    collated `ORDER BY` always materializes + re-sorts §1c), so a skewed read is already
    correct-for-the-loaded-version. The "read-only heap-scan degradation" of
    [compatibility.md §8](compatibility.md) is therefore the *existing* execution path, not new code.
  - **Writes to a `Skewed` table are refused (`XX002`).** Inserting/updating/deleting would encode
    keys under the loaded version into a B-tree built under the file's pinned version — mixing two
    orderings in one tree corrupts it. A table is read-only if its PK, any text column, or any index
    references a `Skewed` collation; the error names the collation + both versions. **Per-table
    granularity** (a `C`-PK table with one skewed secondary index is wholly read-only — finer
    per-index gating is a follow-on).
  - **The per-database default's verdict** is reported via `db.Collations()` (a `verdict` field added
    to `CollationInfo`), so skew is legible before a write fails.

  **Two narrowings, both deliberate (relaxable later):** (a) an **entirely absent** referenced
  collation (no loaded bundle provides the name) **refuses the whole open** with `XX002` (was a bare
  `42704`) rather than opening the rest of the database read-only — the conservative resolution of
  [compatibility.md §12](compatibility.md) open decision #3, since the per-object "open succeeds,
  degrade" path would need the in-memory table to tolerate having no data. (b) The
  **`REINDEX`/`ALTER … COLLATION UPGRADE` migration** that rebuilds a skewed table's collated keys
  against the loaded version and re-pins the stamp — clearing the skew so the table is read-write
  again — is **split to a follow-on**; until it lands a skewed file is read-only (a host can still
  rebuild by recreating). Skew has **no PostgreSQL analog** (PG's `collversion` posture is the
  opposite — host-OS-drift, §15), so the behavior is verified by **per-core unit tests** (the verdict
  is a deterministic pure function — every core computes the identical verdict, the §10 cross-core
  contract — and the write-block + read-correctness), not the oracle corpus (CLAUDE.md §10).
- **2e — real version-pinned root + first tailoring** ✅ *landed (all three cores + Ruby)*: the
  `dev-*` fixtures are replaced in the production **vendored** set by the real CLDR-tailored DUCET
  root — `unicode` (UCA/UCD **17.0.0**, CLDR 48, `spec/collation/17.0.0/root.allkeys` ≈ the CLDR
  `allkeys_CLDR.txt`, the table ICU/PostgreSQL actually use) — plus `es` (root + `&N<ñ<<<Ñ`, the one
  CLDR tailoring that fits the current single-character rule subset). The compiler's working map went
  `Vec → HashMap` so it ingests the ~39 k-mapping root in O(n) (build-time only; the output is sorted,
  so the `.coll` bytes and the dev vectors are unchanged). The `.coll` (~0.3 MB each, the §13 tier-2
  budget) is embedded by every core (Rust `include_bytes!`, Go `//go:embed`, TS base64) and is
  byte-identical; the `dev-*` fixtures survive **only** as the small `compiler.toml`/`sortkey.toml`
  vectors. Orderings are oracle-checked against `postgres:18`'s ICU (`ä` near `a`, lowercase before
  uppercase, `es`: `'nz' < 'ña'` — ñ a distinct letter; root: `'ña' < 'nz'` — ñ = n+accent). Pinned to
  Unicode 17.0 (the current version; what PostgreSQL 19's ICU will use) — the curated common code
  points are version-stable, so the orderings still match the live oracle's ICU 16.0. CJK and other
  `@implicitweights` ranges raise `0A000` (implicit weights deferred). **Still pending:** the footprint
  tiers (§13), implicit weights / the CJK tier-3 root, and the broader tailorings (sv/da/de needing the
  deferred LDML `[before]`/expansion/contraction features + a real weight allocator — the dense
  insertions exhaust the current midpoint allocator).

**Slice 3 — host-loaded Unicode-data bundle** (this revision; **not yet built**), in dependency
order. This **supersedes** the slice-2 "footprint tiers / `include_bytes!` embed" still-pending items
above: collation tables are no longer compiled into the binary at a build *tier*, but loaded from a
host-supplied `JUCD` bundle (§9/§13), and casing follows collation out of the binary (§16). It is a
**delivery** change — the `.coll` / table / executor / sort-key encoding and the `format_version` 18
file entry are all retained (§5), so the on-disk goldens do not move.

- **3a — `JUCD` bundle byte-format spec + vectors** ✅ **landed:** authored
  [../collation/README.md §5](../collation/README.md) (header, manifest, property/root/tailoring
  sections, the sparse-delta representation, the load-time `merge`), plus the bundle round-trip and
  the `merge(root, delta).table == full.table` vectors (§10).
- **3b — the builder tool** ✅ **landed** (the casing half awaits 3e): `build_collation_bundle`
  assembles the committed `.coll` set into a `JUCD` bundle (shared root + deltas), with `--preset`
  `casing-only` / non-CJK / everything (§13) and `--out`; it writes the canonical
  `spec/collation/fixtures/unicode.jucd` at `non-cjk`. Rust-only build-time tooling, compiled out of
  production (§4.1). `casing-only` is recognized but deferred — its property section is 3e (§16).
- **3c — the load seam** ✅ **landed:** `db.LoadUnicodeData(bytesOrReader)` in all three cores
  (privileged, bytes/reader, **not** SQL-reachable, no engine I/O — §11, [api.md §10](api.md));
  `resolveCollation` searches the engine-global **loaded** set (in load order) instead of a compiled-in
  embed; the unconditional `include_bytes!` / `//go:embed` / base64 embed is **removed** (embedding is
  now a host choice — the host hands the same bytes to `LoadUnicodeData`). `db.Collations` (referenced)
  + `db.LoadedCollations` (loaded set).
- **3d — root + delta + load-time merge** ✅ **landed:** the cross-core byte-identity piece (§9) — the
  bundle ships the root once + per-locale deltas, and `LoadUnicodeData` merges them into the table the
  executor already expects, gated by the `merge == full` vectors (§10).
- **3e — the ASCII-casing baseline + property section** ✅ **landed:** `upper(text)`/`lower(text)`
  (the text overload of the range accessors — the resolver branches on the argument type) and `ILIKE`
  in all three cores, with the casing kernels taking the resolved property table explicitly so the
  ASCII baseline stays deterministically testable (§16). The bundle's **property/casing section** is
  populated from [../collation/17.0.0/casing.txt](../collation/17.0.0/casing.txt) (UCD 17.0.0 simple +
  unconditional special mappings) via `compile_casing` + the builder (§4.1); the bare binary still
  carries none (the ASCII baseline). Simple casing is oracle-clean vs `postgres:18`; the expanding
  SpecialCasing (`ß`→`SS`) and the ASCII-baseline passthrough are documented divergences from glibc,
  in per-core unit tests (§15, CLAUDE.md §10). `initcap` (word-boundary titlecasing) remains deferred.

Slice 3 lands **with or behind** the [compatibility.md](compatibility.md) manifest (§2d) for the
graded version-skew verdict, exactly as reference-only did.

**Possible later follow-ons** — **none scheduled or committed**; recorded as candidate
directions the machinery leaves open, *not* a roadmap or a TODO list. Each would be its
own slice if and when there is a reason to pursue it:

- **`LIKE` / pattern matching under a non-`C` collation** (§7) — lift the byte-semantics
  narrowing.
- **CLDR `shifted` variable weighting** per locale (§6) — refine away `non-ignorable`, pinned
  to the oracle.
- **Nondeterministic collations** (case/accent-insensitive *equality*, §6) — the big one:
  forces the UNIQUE-collision / DISTINCT / GROUP BY / hashing / pattern paths to be handled.
- **CJK (Han) collation** (§13 "everything" outlier) — authoring the multi-MB tailoring data; once
  authored it is the **everything** preset, a per-deployment footprint choice (§13), not a per-file cost.

Because the only runtime load is of jed's **own pinned bundle** (never host data), there is **no
host-collation loading surface to design** — no `CREATE COLLATION … FROM HOST | FROM DEFINITION`, no
host-ICU import. The only collation surface in a production build *references* an already-loaded
collation by name (§1); producing the bundle is the build-time pipeline + builder tool (§9), and
loading it is the single privileged `db.LoadUnicodeData` call (§4).

## 15. Divergences from PostgreSQL

Recorded per CLAUDE.md §1:

- **Default column collation is the per-database default recorded in the file** (itself `C` at
  creation, settable, §1) — **not** the host `LC_COLLATE` and **not** a hard-wired constant.
  (Reason: determinism + no ambient-locale dependency, CLAUDE.md §8/§10.)
- **Collations are loaded from a jed-produced bundle, not from the OS** (§2/§9); PG resolves
  collations from the OS/locale environment at runtime. jed reads the host **only at build time** to
  *produce* the bundle; the running engine has no collation host seam — loading jed's **own pinned
  bytes** is not a host read. (Reason: cross-core determinism — three cores' host ICU disagree on day
  one, §2 — plus keeping every runtime use pure.)
- **jed produces and ships its own compiled collation tables in a host-loaded bundle** (§9/§13); PG
  links the host ICU/glibc. (Reason: a deterministic, growable, version-pinned set jed owns, whose
  footprint is the deployer's choice, not the build's.)
- **The database file *references* its collations by name + `(unicode, cldr)` version; it never
  stores the table** (§3/§5). This *looks* like PG's `collversion` posture but is the opposite in
  substance: PG references the **host OS's drifting** library (silent-corruption risk); jed
  references **loaded, pinned, version-stamped** data and catches any skew with a graded open-time
  verdict (§12, [compatibility.md](compatibility.md)) — so jed still has **no
  collation-corruption-on-upgrade failure mode**, the central divergence. The cost is that a Unicode
  bump makes an old file *read-only until `REINDEX`* on the new binary (§12), where PG's baked-nothing
  model and jed's old baked model each avoided that in their own way.
- **Collated indexes store UCA sort keys** (memcmp-ordered, §8); PG stores the original and
  compares with a runtime comparator. (Reason: the single-`memcmp`-order storage contract,
  [encoding.md §1](encoding.md).)
- **Only deterministic collations in the first slice** (§6/§14); PG ships nondeterministic ICU
  collations from the start.
- **No `CREATE COLLATION` and no host-collation import** (§14); a collation comes from a **loaded jed
  bundle** or it does not exist for that database — there is no SQL DDL to define one and no host-ICU
  import path (the only load is the privileged `db.LoadUnicodeData` of jed's own pinned bytes, §4).
- **The bare binary is `C` / ASCII-only, like stock SQLite** (§16); PG ships Unicode casing/collation
  linked to the OS. jed's `upper`/`lower` fold ASCII only until a Unicode property bundle is loaded —
  full Unicode casing and linguistic collation are opt-in data, not built into the binary.

Where jed *does* provide a collation, its **ordering matches PostgreSQL's same-locale ICU ordering**
for the supported levels (the conformance default, §10) — the divergences above are about *which*
collations exist, *where* their data lives, *how* it is delivered, and *how* keys are stored, not
about getting a supported locale's order wrong.

## 16. Unicode property data and casing — the bare-binary ASCII baseline

Casing (`upper`/`lower`/`initcap`, `ILIKE`, and later `normalize`/regex) needs the Unicode Character
Database, which — like collation — is **versioned** (new code points get case mappings in each
release). So casing follows the same rule as collation: **the bare binary carries no Unicode property
tables; they ride the loaded bundle.** This is the SQLite model — stock SQLite folds **ASCII only**
and Unicode casing is the optional ICU extension — and it is what lets a `C`/ASCII-only database pin
**no Unicode version at all** (§3).

> **Status: LANDED (Slice 3e).** `upper(text)`/`lower(text)` and `ILIKE` are implemented in all three
> cores ([functions.md §9](functions.md)); `lower`/`upper` are now the **text overload** of the range
> accessors (the resolver branches on the argument type). The bundle's **property/casing section** is
> populated — compiled from [../collation/17.0.0/casing.txt](../collation/17.0.0/casing.txt) (UCD 17.0.0
> case mappings: 2933 simple + 103 unconditional special) and packed into `unicode.jucd` by the builder
> (§4.1). `initcap` (word-boundary titlecasing) and `normalize`/regex remain deferred follow-ons. The
> contract below is what the cores implement.

- **The ASCII baseline (built in, table-free, eternal).** With **no** property section loaded,
  `upper`/`lower` fold **ASCII `a`–`z`/`A`–`Z` only** and pass every other code point through
  unchanged (`upper('café') → 'CAFé'`) — exactly stock SQLite's default. ASCII folding is a *branch*,
  not a table, so it is free, deterministic, and **version-independent** (the ASCII case mappings are
  fixed forever). `ILIKE` and any case-insensitive identifier matching use the same ASCII rule
  (identifier folding is already ASCII-only, [grammar.md §3](grammar.md)). So the bare binary's casing
  is **always available** and makes **no Unicode-version promise**.
- **Full Unicode casing (the loaded property section).** When a bundle's **property section** is
  loaded (§9/§13), `upper`/`lower`/`initcap` fold via the Unicode simple case mappings + SpecialCasing
  (e.g. `ß`→`SS`), under the bundle's single `(unicode_version)`. Like collation, this is jed's **own
  pinned data**, byte-identical cross-core, deterministic-given-the-bytes — **not** a host read and
  **not** a determinism-ledger exception (§11).
- **One version axis, one bundle.** The property tables share the **`(unicode_version)`** axis with
  the collation root and live in the **same `JUCD` bundle** (§13), so casing and collation can never
  disagree on version. A `casing-only` host loads just the property section (no root); a non-CJK or
  everything bundle includes it alongside the collation sections.
- **Normalization is deferred and bigger.** `normalize()` (NFC/NFD — decomposition mappings +
  canonical combining classes) is a **larger** dataset than case mappings; when it lands it is an
  additional property-section table on the same version axis, **not** part of the ASCII baseline. The
  bundle format (README §5) reserves room for it; the first property section ships **case mappings
  only**.
- **The versioned-key hazard registers into the manifest.** A functional index on `lower(x)` or a
  `GENERATED ALWAYS AS (lower(x))` column stores a casing result, so it is a "stored bytes from a
  versioned computation" — including the **regime** distinction (an index built under the ASCII
  baseline vs. under Unicode-`X` casing). This is the same problem
  [compatibility.md](compatibility.md) unifies for collation and built-in drift: the casing regime +
  `(unicode_version)` is a manifest entry, and a regime change degrades the index to the graded
  read-only heap-scan verdict rather than silently re-folding. (Plain `SELECT upper(x)` stores
  nothing and has no such hazard.)
