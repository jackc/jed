# Schema introspection — design

> How a query (and through it, a host) discovers what a database contains: the decision —
> **`jed_`-prefixed virtual catalog relations**, scoped per database by the existing qualifier —
> plus the **`jed_` name reservation** that keeps their namespace clear (§4, **implemented**).
> Five relations are implemented — **`jed_tables` + `jed_columns`** (slice I1 — §5),
> **`jed_indexes` + `jed_constraints`** (slice I2 — §5.1), and **`jed_statistics`** (P9 — §5.2);
> `jed_sequences` / `jed_types` remain
> designed-only (I3).
> [attached-databases.md](attached-databases.md) §3 owns the qualifier model the scoping rides;
> [session.md](session.md) owns the privilege gating; [api.md](api.md) §7 owns the host handle —
> which deliberately exposes **no** introspection convenience (the old `table_names()` was removed
> once these relations landed; §6); [grammar.md](grammar.md) §3 owns the identifier rules the
> reservation leans on. When a decision here changes, update [TODO.md](../../TODO.md) and those docs
> in the same edit.

## 1. The problem, and the decision

The engine's only schema introspection *was* the host-API `table_names()` (api.md §7) — a
sorted list of table names, nothing more. A host could not ask *from SQL* what tables exist, what
columns a table has, or what type a column is; a generic tool (a REPL, a migration checker, an
admin UI over an untrusted session) had no surface at all. CLAUDE.md §1 sets the bar: SQL is the
primary surface and **everything must be reachable through it** — introspection included. With the
relations below shipped, `table_names()` was **removed** (§6): SQL is now the whole surface.

**Decision (2026-07-04): introspection is a family of `jed_`-prefixed, read-only, computed
catalog relations in the ordinary relation namespace** — `jed_tables` and `jed_columns` first,
then `jed_indexes` / `jed_constraints` / `jed_sequences` / `jed_types` (§5) — resolved like any
table and scoped to a database by the existing qualifier: unqualified = the implicit scope
(`main`), `temp.jed_tables` = the session temp domain, `reports.jed_tables` = the attachment
(attached-databases.md §3). They are **not stored**: rows are derived at execution from the
pinned catalog snapshot of the qualified database, so there is no on-disk change, no
`format_version` bump, and files stay self-describing.

Two parts, staged deliberately:

1. **The `jed_` name reservation** (§4) is **normative and implemented now**, ahead of the
   relations, so no database in the wild can ever contain a user relation the future built-in
   set collides with. This is the cheap, urgent half.
2. **The relations** (§5) are specified here and land as ordinary vertical slices (§8).

## 2. Prior art — why this shape

- **SQLite** (the deployment-model north star) is the closest precedent and a cautionary tale in
  one: it began with `sqlite_schema` (a per-database relation, reached as `aux.sqlite_schema` for
  an attached database) plus non-composable `PRAGMA table_info(…)` statements, and then spent
  3.16.0 adding **pragma table-valued functions** (`SELECT * FROM pragma_table_info('t')`)
  because users kept needing to filter, join, and aggregate over metadata. The lesson: make
  introspection a *relation* from the start. SQLite also reserves the `sqlite_` name prefix —
  the precedent for §4.
- **DuckDB** exposes `duckdb_tables()` / `duckdb_columns()` / … as its native surface — a
  prefixed relation family — with `information_schema` views layered above for compatibility
  (it can: DuckDB has real schemas).
- **Firebird** (historically schema-less, jed's structural cousin) uses prefixed relations in
  the flat namespace: `RDB$RELATIONS`, `RDB$RELATION_FIELDS`.
- **PostgreSQL** is `pg_catalog` tables + `information_schema` views over them; CLAUDE.md §1
  explicitly disclaims `pg_catalog` fidelity ("we own our surface").
- Host-level metadata APIs (JDBC `DatabaseMetaData`, ADO.NET `GetSchema`) are, in every serious
  engine, wrappers **over** the SQL introspection surface — the layering §6 keeps.

The pattern: engines without schemas expose introspection as prefixed relations in the flat
namespace, scoped per database by the database qualifier. That is exactly the shape jed's
attached-database model already provides for free.

## 3. Rejected alternatives (recorded)

- **`information_schema`** — rejected. It is a *schema*, and jed has none: the qualifier
  position is a **database** (attached-databases.md §3), so `information_schema.tables` parses
  as table `tables` in a database named `information_schema`; making it work would mean
  reserving the name and special-casing a schema-that-is-not-a-database into the qualifier
  grammar. Worse, the per-attachment form is inexpressible — `reports.information_schema.tables`
  is a three-part name the grammar deliberately excludes (§3's "the database qualifier never
  appears in column position" has a sibling: no three-part table names). Its SQL-standard column
  shape also presumes catalogs and schemata jed would fill with fakes. **Recorded as a
  deliberate PG divergence** per CLAUDE.md §1: jed ships no `information_schema` and no
  `pg_catalog`; if external tooling ever justifies it, standard-shaped views can be layered
  *above* the `jed_` relations (DuckDB's move) — the door stays open, nothing is planned.
- **Set-returning functions** (`SELECT * FROM jed_tables('reports')`) — rejected, narrowly. The
  FROM-clause-function machinery partially exists (json-table.md's C0 facility), but it requires
  a column-definition list (catalog functions would need a new fixed-row-type form), the target
  database becomes a runtime string instead of a resolved qualifier (a typo is a runtime error,
  not a resolution error; snapshot pinning is less legible), and gating moves from per-table
  `SELECT` to function `EXECUTE`. Relations dominate on every axis that matters here.
- **Functions returning `json`/`jsonb`** — rejected. It discards the type system on the one
  surface that *describes* the type system, and pushes parsing onto every consumer. A caller
  who wants JSON can wrap the relations with the existing `jsonb` surface.
- **Host-API-only introspection** — rejected. It violates CLAUDE.md §1 (everything reachable
  through SQL), gets reimplemented N ways per core *and* per binding (Ruby gem, WASM, future
  wraps) outside the conformance corpus's differential net, and gives untrusted-session tooling
  (which sees only SQL) nothing.

## 4. The `jed_` name reservation — normative, implemented

**Rule.** A **user-supplied** name for an object in the **relation namespace** — a table
(persistent, `TEMP`, or in an attached database), an index, or a sequence
([sequences.md](sequences.md) §2: one shared namespace) — or in the **type namespace** (a
composite type, [composite.md](composite.md)) must not begin with `jed_`. The comparison is
**case-insensitive** (`JED_x` is rejected): identifier resolution folds case and there is no
quoted-identifier escape (grammar.md §3), so no spelling smuggles the prefix past the check.
Violation is **`42939 reserved_name`** (PG's own code — PG uses it for the `pg_` schema prefix),
message `{kind} name {name} is reserved (the jed_ prefix is reserved for system objects)` with
*kind* ∈ `table` / `index` / `sequence` / `type` / `constraint` and the name as written.

**Checked sites** — every statement that introduces a user-supplied name into either namespace:

| Site | Checked name |
|---|---|
| `CREATE TABLE` (all scopes: bare, `TEMP`, `main.`/`temp.`/attachment-qualified) | the table name |
| `CREATE INDEX name ON …` | the **explicit** index name only |
| `CREATE SEQUENCE` (incl. `IF NOT EXISTS` — reservation is not a collision, so it is **not** suppressed) | the sequence name |
| `CREATE TYPE … AS (…)` | the type name |
| `ALTER SEQUENCE … RENAME TO` | the **new** name |
| named `UNIQUE` / `EXCLUDE` constraint (`CONSTRAINT n UNIQUE (…)`, table- or column-level) | the constraint name — the constraint **is** its backing index (constraints.md §5, gist.md), so the user-written name enters the relation namespace |
| `ALTER TABLE … RENAME CONSTRAINT … TO n` | the new name **only when** the target is `UNIQUE` / `EXCLUDE`; CHECK / FK remain table-local and exempt |

**Engine-generated names are exempt.** A serial column's owned sequence `<table>_<col>_seq`
(sequences.md §12.2) and an unnamed index's auto-name `<table>_<cols>_idx` (indexes.md §2) are
derived from already-validated user names — a table legally named `jed` yields a sequence
`jed_id_seq` and an index `jed_a_idx`, both fine. The exemption is safe because jed controls
future built-in names: **no built-in catalog relation will ever carry an engine-auto-name
suffix** (`_seq` / `_idx` / `_key` / `_pkey` / `_check`); the built-in set is the fixed,
documented family in §5.

**Check order.** The reserved-name check sits **with each site's namespace-collision check**,
immediately before it (the point where `42P07` / `42710` would be raised). Every established
validation precedence is preserved — e.g. `CREATE INDEX jed_i ON nosuch (a)` still reports
`42P01` (table existence precedes name checks, the order create_index.test pins). Ordering
between `42939` and `42P07` for the *same* name is unobservable by construction: a reserved
name can never be in the catalog.

**Deliberately NOT reserved** (each considered):

- **Column names** — no collision surface: columns live per-table, and no built-in will ever
  occupy a user table's column namespace. (PG likewise does not reserve `pg_` columns.)
- **`CHECK` and `FOREIGN KEY` constraint names** — these own no backing relation (a CHECK is a
  predicate, an FK owns no B-tree — constraints.md §4/§6), so they live only in the per-table
  constraint namespace, which hosts no built-ins; and auto-names derived from a table named
  `jed` (`jed_x_check`, `jed_a_fkey`) must stay legal. Named `UNIQUE`/`EXCLUDE` constraints are
  the deliberate exception above — their names ARE relation names.
- **Function names** — the function catalog is curated and built-in-only (CLAUDE.md §13); there
  is no user-supplied function name to reserve. A host-registered function is the host's
  namespace and the host's problem (the §13 host-extension boundary).
- **Attachment names** (`db.attach`) — the qualifier namespace already reserves `main`/`temp`
  (attached-databases.md §7, `42710`), and no `jed_` *qualifier* will ever exist: catalog
  relations are reached through each database's own namespace (§5), never through a synthetic
  catalog database.

**Why now, before any relation ships.** The reservation must predate real-world databases: a
file created *after* this change structurally cannot contain a user relation that a future
built-in collides with. Files created before it could (nothing forbade `CREATE TABLE
jed_tables` until now); §5's built-in-first resolution rule defines what happens to such a
legacy name, and the affected set is expected to be empty in practice.

**Divergence note (CLAUDE.md §1).** PostgreSQL reserves `pg_` for *schemas* only, not relation
names — it has a schema to hide its catalog behind. jed has no schemas, so the reservation must
live in the relation and type namespaces themselves; SQLite's `sqlite_` prefix is the model.
Recorded here per the §1 rule.

Conformance: `suites/ddl/reserved_names.test` (rides the existing DDL capabilities — the
reservation is part of each DDL statement's semantics, not an optional feature).

## 5. The catalog relations — `jed_tables` + `jed_columns` implemented (I1)

**Model.** Each catalog relation is a **read-only computed relation**: at execution its rows are
derived from the **pinned catalog snapshot** of the database it is qualified into — never
stored, never maintained, no on-disk presence. This does not breach §9's "no external/virtual
row sources" guarantee: that rule keeps files reopenable without external code or data, and a
catalog relation is derived entirely from the file's *own* catalog. A spanning query mixing
`jed_tables` and `reports.jed_tables` is, like any spanning query, a pure function of the
per-database pinned snapshots (attached-databases.md §5). In every core the relation rides the
existing computed-relation (SRF-plan) execution shape, so each "computed, not scanned" gate —
no store, no index pushdown, no PK scan order, excluded from the streaming/vectorized fast
lanes — holds by construction rather than by N new checks.

**Resolution** (implemented, pinned by `suites/introspection/`). Built-in catalog names resolve
in every database's relation namespace, **checked before the user catalog** (deterministic, PG's
`pg_catalog`-first shape) and **after a statement-local CTE** — `WITH jed_tables AS (…)` shadows
the built-in, matching PostgreSQL's CTE-over-catalog resolution (oracle-checked against
`pg_tables`). An **unqualified** name reads the **implicit scope (`main`)** — never the temp-first
walk, since the built-in exists in *every* database and the scope must be pinned;
`temp.jed_tables` / `<attachment>.jed_tables` read that database's snapshot, and an unknown
qualifier is `42P01` (the ordinary "database … is not attached" wording). A FROM **alias renames
the relation only, never its column** (no single-column function-alias rule — these are
relations, not functions). Post-§4 a user-catalog collision is impossible; for a legacy file that
already contains a user relation named `jed_tables`, the built-in wins and the user relation
becomes unreachable by name (its data is intact and re-reachable by dump/recreate under a legal
name) — accepted and recorded rather than allowing shadowing, which attached-databases.md §3
deliberately banned.

**Read-only** (implemented). A mutation or DDL target naming a catalog relation is **`42809
wrong_object_type`**, checked by *name* before qualifier validation (the built-in resolves in
every database, so the rejection is scope-independent): INSERT / UPDATE / DELETE and `CREATE
INDEX … ON` raise `cannot modify system relation "jed_tables"`; `DROP TABLE` raises `cannot drop
system relation "jed_tables"`, and **`IF EXISTS` does not suppress it** (a kind rejection, not a
missing name). `CREATE` of any `jed_`-prefixed name stays the §4 reservation (`42939`). A
`REFERENCES jed_tables` FK parent stays `42P01` (the parent lookup resolves user tables only — a
catalog relation has no key to reference).

**Self-exclusion.** Catalog relations list **user objects only** — they do not list themselves
or each other (the doc-hidden `tooling` catalog accessors the CLI reaches, api.md §6, are likewise
user-objects-only).

**Privileges** (implemented). A catalog relation is gated exactly like a user table: per-table
`SELECT` under the session envelope (session.md), no special case — the privilege gate treats the
built-in names as existing relations, so an explicit-grant session (`default_privileges = NONE`)
raises `42501` without a `grant: SELECT ON jed_tables`. Whether an untrusted session may see the
schema is thereby a host policy decision made with existing machinery. Secure by default under
explicit-grant sessions.

**Determinism & cost** (implemented; pinned in [cost.md](cost.md) "`generated_row`"). Content is
a pure function of the pinned snapshot (CLAUDE.md §10). Rows are generated in ascending
lowercased-name order (jed_columns: then ordinal) — a deterministic internal order with no
map-iteration leak; the observable contract is the **multiset**, row order without `ORDER BY`
stays unspecified (§8 — the corpus compares `rowsort`). Each produced row charges one
**`generated_row`** at the source, under the meter guard (a ceiling aborts mid-generation,
CLAUDE.md §13), and a catalog scan charges **zero `page_read` / `storage_row_read`** (the catalog
is resident by construction — pager.md's catalog residency). `EXPLAIN` renders the leaf as
`Catalog Scan jed_tables (db=<scope>)`.

**Column sets** (implemented as proposed):

```
jed_tables(
  name        text NOT NULL      -- canonical (CREATE TABLE-spelled) table name
)

jed_columns(
  table_name  text NOT NULL,     -- canonical owning-table name
  name        text NOT NULL,     -- canonical column name
  ordinal     i32  NOT NULL,     -- 1-based, CREATE TABLE order
  type        text NOT NULL,     -- canonical type rendering (below)
  not_null    boolean NOT NULL,  -- declared NOT NULL or PRIMARY KEY member
  pk_ordinal  i32                -- 1-based position in the PRIMARY KEY, in KEY order
                                 --   (constraints.md §3 — may differ from declaration order);
                                 --   NULL if not a member
)
```

**The canonical `type` text** (a compatibility surface from the moment it ships; every
renderable type is pinned in `suites/introspection/jed_columns.test`): the scalar's canonical
name (`i32`, `text`, `boolean`, `decimal`, `f64`, `jsonb`, …) with the **typmod applied at the
leaf** — `varchar(10)`, `decimal(8,2)`; a composite renders **its name as created**; a range its
canonical id from ranges.toml (`i32range`, `numrange`, …); an array appends `[]` to its element's
rendering (`i32[]`, `addr[]` — and when the element-typmod narrowing lifts, `varchar(5)[]`).

Deliberately minimal: no row-count column yet (v28 persists the fact for planning, but exposing it is
a separate introspection-surface decision), no `DEFAULT` rendering yet (it needs a pinned canonical
expression-text form; deferred to a later column addition). **Growth is by adding columns**, so
consumers should select columns by name, not `SELECT *` positionally — documented at the
relation, PG's own catalog posture.

## 5.1 `jed_indexes` + `jed_constraints` (I2) — implemented

The same model as §5 (read-only computed relations, scoped by the qualifier, riding the SRF-plan
shape, self-excluding, `SELECT`-gated, `42809` on a write target, one `generated_row` per produced
row): two more relations that describe a table's **indexes** and its **constraints**. Both are
resolved and gated by the identical `jed_`-name funnel as `jed_tables` / `jed_columns` — adding
them was adding two entries to the built-in-name classifier plus two row generators, so every
"computed, not scanned" gate holds by construction.

The I1 sketch listed these as `jed_indexes (name, table, columns, unique, method)` and
`jed_constraints (…)`; the formal column sets below **refine** that sketch — `table` →
`table_name` (consistent with `jed_columns`), `unique` → `is_unique` (a self-documenting boolean,
DuckDB's `duckdb_indexes` spelling), and `columns` is a **`text[]`** of column names (jed has
first-class arrays, so the member list is a queryable array, not a delimited string). Every column
name and value below is a **compatibility surface** from the moment it ships — pinned by
`suites/introspection/jed_indexes.test` and `jed_constraints.test`.

```
jed_indexes(
  name        text NOT NULL,     -- the index name (relation namespace, original case)
  table_name  text NOT NULL,     -- the canonical owning-table name
  columns     text[] NOT NULL,   -- the indexed column names in index-key order (duplicates included)
  is_unique   boolean NOT NULL,  -- whether the index enforces uniqueness (indexes.md §8)
  method      text NOT NULL,     -- the access method: 'btree' | 'gin' | 'gist'
  predicate   text               -- a partial index's predicate text (canonical, indexes.md §9);
                                 --   NULL for a non-partial index (PG's pg_index.indpred analog)
)

jed_constraints(
  name        text NOT NULL,     -- the constraint name (constraints.md naming)
  table_name  text NOT NULL,     -- the canonical owning-table name
  type        text NOT NULL,     -- 'check' | 'unique' | 'foreign_key' | 'exclude'
  columns     text[],            -- member/local column names: UNIQUE members, FK local columns,
                                 --   or EXCLUDE columns (constraint order); NULL for a CHECK
  expression  text,              -- the CHECK expression text (the persisted canonical token form,
                                 --   constraints.md §4.5 — cross-core byte-identical); NULL otherwise
  ref_table   text,              -- FOREIGN KEY: the referenced (parent) table name; NULL otherwise
  ref_columns text[]             -- FOREIGN KEY: the referenced parent column names (list order);
                                 --   NULL otherwise
)
```

**`jed_indexes` lists every secondary index** in `table.indexes` — a plain `CREATE INDEX`, a
`CREATE UNIQUE INDEX`, the unique index that *backs* a `UNIQUE` constraint (constraints.md §5), and
the GiST index that *backs* an `EXCLUDE` constraint (gist.md §7). A constraint-backing index
therefore appears in **both** `jed_indexes` and `jed_constraints` under the same name — the same
parallel PostgreSQL keeps between `pg_indexes` and `pg_constraint`, and the join key between the two
relations. `is_unique` is the catalog's `unique` flag; `method` renders the index kind;
`predicate` renders a **partial index's** predicate canonical text (indexes.md §9), NULL for a
non-partial index. The primary key owns no index object (its `<table>_pkey` name is not persisted —
constraints.md §5.4), so it is **not** a row here; it is surfaced by `jed_columns.pk_ordinal`.

**`jed_constraints` covers the four kinds the design doc enumerates — CHECK, UNIQUE, FK, EXCLUDE —
and *only* those.** The `PRIMARY KEY` and `NOT NULL` constraints are deliberately absent: they own
no named catalog object (constraints.md §1/§3, §5.4) and are already fully described by
`jed_columns` (`pk_ordinal`, `not_null`). Because a jed **`UNIQUE` constraint *is* its backing
unique index** (constraints.md §5 — there is no separate constraint object, and the catalog cannot
distinguish a `UNIQUE` table constraint from a bare `CREATE UNIQUE INDEX`), `type = 'unique'` lists
**every unique b-tree index**; this is honest to jed's model (a unique index *is* a uniqueness
constraint) and gives a consumer the complete uniqueness picture in one relation. `type =
'exclude'` lists the exclusion constraints (columns = the excluded columns in element order — the
`&&`/`=` operators are a deferred column addition, the §5 "growth by adding columns" rule).
`expression` is populated for `type = 'check'` only, from the persisted canonical expression text
(constraints.md §4.5), which is already cross-core byte-identical, so it needs no new
canonical-form work (contrast the deferred `DEFAULT` rendering of §5).

**Generation order** (deterministic, no map-iteration leak — CLAUDE.md §8; the observable contract
is the multiset, row order without `ORDER BY` unspecified). `jed_indexes`: tables in ascending
lowercased-name order, then each table's indexes in the catalog's ascending lowercased-name order.
`jed_constraints`: tables in ascending lowercased-name order, then per table by **kind** (check,
unique, foreign_key, exclude), each kind already held in ascending lowercased-name order — a fixed
kind order because an FK and a UNIQUE index may share a name within one table (FK names are checked
only against the constraint namespace, constraints.md §6.2), so a global name sort is not a total
order.

## 5.2 `jed_statistics` (P9) — implemented

The same read-only, qualified, computed-relation model exposes one summary row per analyzed column:

```
jed_statistics(
  table_name       text NOT NULL,
  column_name      text NOT NULL,
  analyzed_rows    i64 NOT NULL,
  is_stale         boolean NOT NULL,
  null_count       i64 NOT NULL,
  distinct_count   i64,
  sample_rows      i64 NOT NULL,
  average_width    i64,
  mcv_count        i32 NOT NULL,
  histogram_count  i32 NOT NULL
)
```

`distinct_count` is NULL for a distribution-ineligible type; `average_width` is NULL when the
analyzed population had no non-NULL value. Counts describe the stored analyzed fact, not a
row-count-rescaled current estimate. `is_stale` becomes true after later DML and remains visible
until explicit ANALYZE replaces the column fact ([statistics.md](statistics.md)). Rows generate by
lowercased table name then column ordinal. The relation is independently privilege-gated under the
name `jed_statistics`, charges one `generated_row` per summary, and exposes no typed MCV/histogram
arrays in P9.

**Later relations** (same model, own slices — I3): `jed_sequences` (the six definition fields +
ownership), `jed_types` (composite types + fields). Capability ids `introspect.tables`,
`introspect.columns`, `introspect.indexes`, `introspect.constraints`, `introspect.statistics`, … —
one per relation.

## 6. The host API carries no introspection convenience

**Decision (2026-07-04): the host handle exposes no schema-introspection convenience — introspection
is SQL, full stop.** With the `jed_` relations shipped, the pre-existing `table_names()` catalog read
(api.md §7) is **removed** from the public `Database`/`Session` surface in every core: a host that
wants the table list runs `SELECT name FROM jed_tables`, and everything richer is `jed_columns` /
`jed_indexes` / `jed_constraints`. This is the CLAUDE.md §1 rule taken to its conclusion — a second,
per-language, per-binding metadata surface (reimplemented N ways outside the conformance corpus's
differential net) is exactly the drift §5 exists to prevent, and it gave untrusted-session tooling
(which sees only SQL) nothing anyway. The earlier framing ("`table_names()` stays as a thin
convenience wrapping the relations") is superseded: there is no wrapper, because there is no second
surface to keep consistent.

**What remains is not a convenience but a doc-hidden tooling seam.** `table()` (and
`composite_type()`) survive as the `#[doc(hidden)]` `tooling` introspection accessors the **in-repo
CLI and white-box tests** reach for (api.md §6) — the CLI's `.dump` reconstructs `CREATE TABLE` from
the full catalog `Table`, which the `jed_` relations do not yet fully expose (column DEFAULTs are
still un-rendered in `jed_columns`, §5). These are internal machinery, explicitly not the embedding
API, and a host is never pointed at them. Per core the seam differs by necessity (api.md §6): Rust
keeps `table`/`table_names` as `#[doc(hidden)]` handle accessors because its CLI is a separate crate;
Go drops the `TableNames` wrappers entirely; TS's tools reach `Engine.tableNames()` directly.

**Attachment listing is host-API-only, by design.** Which databases are attached is *handle*
state created by host-API acts (attached-databases.md §2), not database state — so there is no
`jed_databases` relation; the host already holds what it attached. This also keeps every catalog
relation a pure function of one database's snapshot.

**Attachment listing is host-API-only, by design.** Which databases are attached is *handle*
state created by host-API acts (attached-databases.md §2), not database state — so there is no
`jed_databases` relation; the host already holds what it attached. This also keeps every catalog
relation a pure function of one database's snapshot.

## 7. Error codes

| Code | Name | Raised |
|---|---|---|
| `42939` | `reserved_name` | a user-supplied relation/type name beginning `jed_` (§4) — **registered and implemented now** |
| `42809` | `wrong_object_type` | a mutation / `CREATE INDEX` / `DROP TABLE` target naming a catalog relation (§5 — read-only; an existing registered code, this use pinned by I1, extended to the I2 relations by construction) |
| `42501` | `insufficient_privilege` | `SELECT` on a catalog relation without a grant under an explicit-grant session envelope (§5; the ordinary session.md gate) |

The later relations' own errors (if any new arise) are pinned by their implementing slices.

## 8. Slices & status

| Slice | Contents | Status |
|---|---|---|
| **I0** | this doc; `42939` in the error registry; the `jed_` reservation in all three cores; `suites/ddl/reserved_names.test` | ✅ landed |
| **I1** | `jed_tables` + `jed_columns`: resolution funnel interception (CTE-shadow / built-in-first / qualifier scoping), computed-relation execution riding the SRF plan shape, privilege gating, the 42809 read-only rejections, `generated_row` cost pinning (cost.md), `EXPLAIN` `Catalog Scan`, capabilities `introspect.tables`/`introspect.columns`, the canonical-type-text corpus (`suites/introspection/`, 4 files incl. temp + attachment scoping), `/web` docs | ✅ landed |
| **I2** | `jed_indexes` + `jed_constraints` (§5.1): two more built-in-name-classifier entries + two row generators (every gate inherited from I1), the `text[]` member-list columns, the CHECK `expression` from the persisted canonical text, capabilities `introspect.indexes`/`introspect.constraints`, corpus (`suites/introspection/jed_indexes.test` + `jed_constraints.test`), cost.md worked examples, `/web` docs | ✅ **this change** |
| **P9** | `jed_statistics` (§5.2): one summary row per analyzed column, qualified-domain scoping, stale/NDV/width/count visibility, capability `introspect.statistics`, corpus + `/web` docs | ✅ landed with column statistics |
| I3 | `jed_sequences`, `jed_types` | not started |
| — | `information_schema` compat views over the `jed_` relations | door open, **not planned** |
