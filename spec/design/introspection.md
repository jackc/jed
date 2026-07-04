# Schema introspection — design

> How a query (and through it, a host) discovers what a database contains: the decision —
> **`jed_`-prefixed virtual catalog relations**, scoped per database by the existing qualifier —
> plus the **`jed_` name reservation** that keeps their namespace clear (§4, **implemented**;
> the relations themselves are **designed, not yet built** — §5, §8).
> [attached-databases.md](attached-databases.md) §3 owns the qualifier model the scoping rides;
> [session.md](session.md) owns the privilege gating; [api.md](api.md) §7 owns `table_names`,
> the one host-level introspection call that predates this doc; [grammar.md](grammar.md) §3 owns
> the identifier rules the reservation leans on. When a decision here changes, update
> [TODO.md](../../TODO.md) and those docs in the same edit.

## 1. The problem, and the decision

The engine's only schema introspection today is the host-API `table_names()` (api.md §7) — a
sorted list of table names, nothing more. A host cannot ask *from SQL* what tables exist, what
columns a table has, or what type a column is; a generic tool (a REPL, a migration checker, an
admin UI over an untrusted session) has no surface at all. CLAUDE.md §1 sets the bar: SQL is the
primary surface and **everything must be reachable through it** — introspection included.

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

## 5. The catalog relations — designed, not yet built

**Model.** Each catalog relation is a **read-only computed relation**: at execution its rows are
derived from the **pinned catalog snapshot** of the database it is qualified into — never
stored, never maintained, no on-disk presence. This does not breach §9's "no external/virtual
row sources" guarantee: that rule keeps files reopenable without external code or data, and a
catalog relation is derived entirely from the file's *own* catalog. A spanning query mixing
`jed_tables` and `reports.jed_tables` is, like any spanning query, a pure function of the
per-database pinned snapshots (attached-databases.md §5).

**Resolution.** Built-in catalog names resolve in every database's relation namespace, **checked
before the user catalog** (deterministic, PG's `pg_catalog`-first shape). Post-§4 the two can
never collide; for a legacy file that already contains a user relation named `jed_tables`, the
built-in wins and the user relation becomes unreachable by name (its data is intact and
re-reachable by dump/recreate under a legal name) — accepted and recorded rather than allowing
shadowing, which attached-databases.md §3 deliberately banned. Writes to a catalog relation
(INSERT/UPDATE/DELETE, or DROP/CREATE against its name post-resolution) are rejected — exact
code pinned in the implementing slice (PG uses `42809 wrong_object_type` for kind mismatches).

**Self-exclusion.** Catalog relations list **user objects only** — they do not list themselves
or each other, matching what `table_names()` returns today (api.md §7 stays user-objects-only).

**Privileges.** A catalog relation is gated exactly like a user table: per-table `SELECT` under
the session envelope (session.md), no special case. Whether an untrusted session may see the
schema is thereby a host policy decision made with existing machinery — grant `SELECT` on
`jed_tables` or don't. Secure by default under explicit-grant sessions.

**Determinism & cost.** Content is a pure function of the pinned snapshot (CLAUDE.md §10). Row
order is unspecified without `ORDER BY` (§8 — the corpus compares `rowsort`). Execution is
metered with the ordinary row-production units and **zero `page_read`** (the catalog is resident
by construction — pager.md's catalog residency); the exact unit schedule is pinned in
[cost.md](cost.md) by the implementing slice, cross-core-identical like every cost.

**First slice — proposed column sets** (the implementing slice may adjust; changes land here):

```
jed_tables(
  name        text NOT NULL      -- canonical (CREATE TABLE-spelled) table name
)

jed_columns(
  table_name  text NOT NULL,     -- canonical owning-table name
  name        text NOT NULL,     -- canonical column name
  ordinal     i32  NOT NULL,     -- 1-based, CREATE TABLE order
  type        text NOT NULL,     -- canonical type rendering: i32, text, varchar(10),
                                 --   decimal(8,2), i32[], numrange, a composite's name, …
  not_null    boolean NOT NULL,  -- declared NOT NULL or PRIMARY KEY member
  pk_ordinal  i32                -- 1-based position in the PRIMARY KEY; NULL if not a member
)
```

Deliberately minimal: no row counts (a count would force the leaf walks the v25 open work just
removed — storage.md §6), no `DEFAULT` rendering yet (it needs a pinned canonical
expression-text form; deferred to a later column addition). **Growth is by adding columns**, so
consumers should select columns by name, not `SELECT *` positionally — documented at the
relation, PG's own catalog posture. The canonical `type` text becomes a compatibility surface
the moment it ships: the implementing slice pins every renderable type in the corpus.

**Later relations** (same model, own slices): `jed_indexes` (name, table, columns, unique,
method), `jed_constraints` (CHECK / UNIQUE / FK / EXCLUDE, per constraints.md), `jed_sequences`
(the six definition fields + ownership), `jed_types` (composite types + fields). Capability ids
`introspect.tables`, `introspect.columns`, … — one per relation.

## 6. What stays on the host API

`table_names()` remains as-is (api.md §7): a thin convenience, and the precedent that host
metadata surfaces stay **wrappers over** (or trivially consistent with) the SQL relations —
never a second source of truth. Future per-language conveniences follow the JDBC layering.

**Attachment listing is host-API-only, by design.** Which databases are attached is *handle*
state created by host-API acts (attached-databases.md §2), not database state — so there is no
`jed_databases` relation; the host already holds what it attached. This also keeps every catalog
relation a pure function of one database's snapshot.

## 7. Error codes

| Code | Name | Raised |
|---|---|---|
| `42939` | `reserved_name` | a user-supplied relation/type name beginning `jed_` (§4) — **registered and implemented now** |

The relations' own errors (read-only violation, etc.) are pinned by their implementing slices.

## 8. Slices & status

| Slice | Contents | Status |
|---|---|---|
| **I0** | this doc; `42939` in the error registry; the `jed_` reservation in all three cores; `suites/ddl/reserved_names.test` | ✅ **this change** |
| I1 | `jed_tables` + `jed_columns`: resolution funnel interception, computed-relation execution, privilege gating, cost pinning, canonical-type-text corpus, `/web` docs | not started |
| I2 | `jed_indexes`, `jed_constraints` | not started |
| I3 | `jed_sequences`, `jed_types` | not started |
| — | `information_schema` compat views over the `jed_` relations | door open, **not planned** |
