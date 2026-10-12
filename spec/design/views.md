# Views — design

> `CREATE VIEW name [(cols)] AS query` / `DROP VIEW`: a **named, persisted query** that the FROM
> clause references like a table. This doc is the contract all three cores implement in lockstep
> (CLAUDE.md §2). The grammar is in [../grammar/grammar.ebnf](../grammar/grammar.ebnf) and
> [grammar.md](grammar.md) §54. The byte layout is in [../fileformat/format.md](../fileformat/format.md)
> (`format_version` 38). The errors are existing codes in [../errors/registry.toml](../errors/registry.toml).
> PostgreSQL behavior was pinned against the live `postgres:18` oracle (CLAUDE.md §1).
> This first slice is **read-only**: no updatable views, `OR REPLACE`, `CHECK OPTION`, materialized,
> recursive, or temporary views (§12).

A view is the fifth kind of database-level catalog object, after tables, composite types, sequences,
and collation references. It has no storage of its own. Reading a view plans its stored query in
place, exactly as if the query had been written as a derived table ([grammar.md](grammar.md) §42).
Everything below follows from three choices:

1. **The definition is persisted as canonical token text and planned again at every use** (§3).
   This is SQLite's model and jed's CHECK-expression precedent
   ([constraints.md](constraints.md) §4.5). PostgreSQL instead stores a parse tree bound to object
   ids. Text bound by name must be kept bound to the same objects, so §5–§7 freeze `*`, track
   dependencies, rewrite the text on `RENAME`, and re-validate dependents after `ALTER TABLE`.
2. **A view reference is the equivalent derived table** (§4): identical rows, cost, plan, and
   EXPLAIN. The planner gains no view-specific operator.
3. **A view is a privilege boundary, as in PostgreSQL** (§9). Reading a view needs `SELECT` on the
   view alone. Because jed has no persistent owners, the "owner" check happens once, at
   `CREATE VIEW`, against the creating session.

## 1. Surface

```
CREATE VIEW [IF NOT EXISTS] [main.]name [( col [, …] )] AS query
DROP VIEW [IF EXISTS] name [, …] [CASCADE | RESTRICT]
```

- **`query`** is a `subquery_expr`: an optional `WITH` clause, then a `SELECT`, set operation,
  `ORDER BY`, and `LIMIT`/`OFFSET`. Anything a derived-table body accepts is accepted, including
  joins, aggregates, window functions, recursive CTEs, `LATERAL`, set-returning functions, and the
  `jed_` catalog relations. A top-level `VALUES` body is `42601`, because jed has no top-level
  `VALUES` query (PostgreSQL accepts it; this is the general narrowing, not a view rule).
- **The optional column list** renames the view's output columns left to right. A shorter list is
  a partial rename and the rest keep their query names. A longer list is **`42601`** (*CREATE VIEW
  specifies more column names than columns*, PostgreSQL's code).
- **Output names** follow grammar.md §8. A bare column keeps its name, an alias its alias, an
  un-aliased aggregate its function name, and any other expression is `?column?`. Two output
  columns with the same final name are **`42701`** (*column "x" specified more than once*). This
  includes two `?column?`s: `CREATE VIEW v AS SELECT 1, 2` is `42701`, as in PostgreSQL. A view
  has at least one column; a zero-column body is `0A000` (§12).
- **`IF NOT EXISTS`** is a **jed extension** (SQLite has it; PostgreSQL's grammar does not). If the
  name is already taken in the relation namespace, the statement is a silent no-op. The skip is
  decided before the body is parsed into a plan, as for `CREATE TABLE IF NOT EXISTS`
  ([grammar.md](grammar.md) §13). The skipped body is not validated. The `allow_ddl` gate,
  qualifier validation, and the `jed_` reservation still run first.
- **`DROP VIEW`** follows `DROP TABLE`'s shape ([grammar.md](grammar.md) §13). It is two-phase and
  all-or-nothing over a comma list. A repeated name is deduplicated. A missing name is **`42P01`**
  (*view does not exist*) unless `IF EXISTS`, which skips just that name. A name that resolves to
  a table, index, or sequence is **`42809`** (*"t" is not a view*), and `IF EXISTS` does not
  suppress it.
- **Transactional.** `CREATE VIEW` and `DROP VIEW` are DDL in the §3 staging model. They roll back
  with their transaction, are refused in a read-only transaction (`25006`), and are gated by the
  session's `allow_ddl` (`42501`, [session.md](session.md) §5).
- **Cost is zero** for both statements. Planning is unmetered, and the statements read no rows
  ([cost.md](cost.md)).

### 1.1 Validation order at `CREATE VIEW` (deterministic, PG-matched)

1. The `allow_ddl` gate (`42501`), then the database qualifier: `main` is accepted, `temp` and any
   attachment are `0A000` (§10), and an unknown qualifier is `42P01`.
2. The `jed_` reservation (`42939`, kind `view`, [introspection.md](introspection.md) §4).
3. `IF NOT EXISTS` and a taken name: a silent no-op, and nothing below runs.
4. The body is planned in the isolated view scope (§4.1). Every resolution error surfaces here:
   `42P01`, `42703`, `42702`, `42883`, `42804`, and so on. A `$N` is **`42P02`** (*there is no
   parameter $N*, PostgreSQL). A data-modifying `WITH` is **`0A000`** (*views must not contain
   data-modifying statements in WITH*, PostgreSQL). A temp or attached relation is `0A000` (§10).
5. Column-list arity (`42601`), then duplicate final output names (`42701`), computed from the
   body as planned in step 4.
6. `*` is expanded and the expanded text is verified (§5).
7. The creating session's `SELECT` privilege on every relation the body reads (`42501`, §9).
8. A taken name is **`42P07`** (*relation "v" already exists*). PostgreSQL also reports body errors
   before the name collision, so `CREATE VIEW v AS SELECT * FROM nosuch` is `42P01` even when `v`
   exists.
9. The catalog entry is written. A definition whose entry does not fit one catalog page is
   `0A000` (the catalog's one-page-per-entry rule, format.md *Catalog*).

## 2. Namespace

A view lives in the **relation namespace**, which tables, indexes, sequences, and named
`UNIQUE`/`EXCLUDE` constraints already share ([sequences.md](sequences.md) §2). Creating any of them
under a view's name is `42P07`, and creating a view under any of their names is `42P07`. The
`jed_` reservation applies with kind `view`. A view name never collides with a composite type,
because jed has no implicit per-relation row types ([composite.md](composite.md)).

Statements that name a relation of a specific kind treat a view as the wrong kind:

| Statement | On a view | PG |
|---|---|---|
| `DROP TABLE v` (also `IF EXISTS`) | `42809` *"v" is not a table* | same |
| `DROP INDEX v` | `42809` | same |
| `DROP SEQUENCE v`, `ALTER SEQUENCE v`, `nextval('v')` | `42P01`, as for a table: sequence statements look up sequences only | PG: `42809` |
| `INSERT INTO v`, `INSERT … ON CONFLICT` | `55000` *cannot insert into view "v"* | PG updates simple views |
| `UPDATE v` | `55000` *cannot update view "v"* | PG updates simple views |
| `DELETE FROM v` | `55000` *cannot delete from view "v"* | PG updates simple views |
| A writable CTE targeting `v` | `55000`, as above | same rule |
| `CREATE INDEX … ON v` | `42809` *cannot create index on relation "v"* | same |
| `REFERENCES v` (a foreign-key parent) | `42809` *referenced relation "v" is not a table* | same |
| `ALTER TABLE v RENAME TO …` / `RENAME COLUMN …` | `0A000` (a follow-on, §12) | PG renames |
| Any other `ALTER TABLE v …` action | `42809` | same |
| `ALTER VIEW …` | `0A000` (a follow-on, §12) | PG supports |
| `ANALYZE v` | a no-op success | PG warns and succeeds |
| `DROP VIEW t` (`t` a table, index, or sequence) | `42809` *"t" is not a view* | same |

`55000` (`object_not_in_prerequisite_state`) is PostgreSQL's code for writing to a view that is
not automatically updatable. Every jed view is non-updatable in this slice, so jed raises it for
every view. PostgreSQL accepts writes to a simple single-table view, and that is the recorded
divergence (§12). The rejection happens when the DML target is resolved, before any column, value,
or `RETURNING` check.

## 3. Persistence (`format_version` 38)

A view is a catalog entry with `entry_kind = 5` ([../fileformat/format.md](../fileformat/format.md)
*View entry*). It records:

- the view **name**;
- the **output column names**, in order: the column list applied over the body's output names,
  frozen at `CREATE VIEW`;
- the **definition**: the canonical token text of the body after `*` expansion (§5);
- the **dependency list**: every table and view the body reads, with, for each table, the exact set
  of its columns the body references (§6).

**Canonical text.** The body is the token sequence after `AS` to the end of the statement. Each
token is rendered by the closed per-token table in format.md *Check-expression text* and the tokens
are joined with single spaces. A bare word therefore renders folded (keywords included), a quoted
identifier renders quoted, and comments are gone. Example:
`CREATE VIEW v AS SELECT "Qty", b FROM t WHERE b IS NOT NULL` stores
`select "Qty" , b from t where b is not null`. Every core renders the same tokens to the same
bytes. The stored text is what `jed_views.definition` shows and what the CLI dump emits (§11).

**Open does not plan.** At open a core parses each definition with its ordinary query parser
(unparseable text is `XX001`) and checks the entry's structure. The dependency list must name
existing tables and views, every listed column must exist in its table, the view-dependency graph
must be acyclic, and the output names must be non-empty and unique. A violation is `XX001`. The
body is **not** planned at open. Planning could fail for reasons that are not corruption, such as
a host function that the reopening host has not registered. Those failures surface when the view
is used, as the ordinary error, and never make a database unopenable.

**Output types are not persisted.** They are derived by planning the body. Every change that could
alter them is either blocked by a dependency (§6) or re-validated (§8), so the types a view
produces are stable for the life of the view.

## 4. Reading a view: the equivalent derived table

A FROM item `name [AS alias [(cols)]]` resolves in this order, which is unchanged except for the
new last step: a statement-local CTE, then a built-in `jed_` catalog relation, then a table, then a
**view**. Tables and views share one namespace, so at most one of the last two matches.

A view reference **is** the derived table

```
( <definition> ) AS <label> ( <output column names> )
```

where `<label>` is the reference's alias, or the view's name when there is none. A column list on
the reference (`FROM v AS x(p, q)`) renames over the view's output names with the derived-table
rules: a shorter list is a partial rename and a longer list is `42P10`. Everything a derived table
does, a view reference does, with exactly the same result:

- **Rows, values, types, errors, and output names** are those of the equivalent derived table.
  Output collation derivation is the body's, so a view over a collated `text` column carries the
  column's collation into an outer `ORDER BY`.
- **Cost** is the derived table's intrinsic inline cost ([cost.md](cost.md) §3). There is no new
  unit and no charge for the view itself.
- **Plan.** The planner treats the reference as the derived table, so every rewrite and pushdown a
  derived table receives applies, including predicate pushdown into the body
  ([planner.md](planner.md) §3.2) and access-path selection inside it. `EXPLAIN` renders the
  reference exactly as it renders the equivalent derived table: a `Subquery <label>` node over the
  body's plan.
- **Ordering.** A body `ORDER BY` orders the body. The outer query's row order is still defined
  only by the outer `ORDER BY` (CLAUDE.md §8).

This equivalence is a cross-core contract. The NoREC generator pins it with a
**view-vs-inlined-derived-table** relation: the same query over `v` and over `(<definition>) AS
v(<cols>)` must produce the same rows and the same cost ([conformance.md](conformance.md) §8).

### 4.1 The isolated view scope

A view body is planned in a fresh top-level scope, both at `CREATE VIEW` and at every use:

- **No correlation.** It sees no enclosing query level and no sibling FROM item, even when the
  reference is inside a subquery or after `LATERAL`. A name the body does not resolve itself is an
  error, never a correlated reference.
- **No outer CTEs.** The referencing statement's `WITH` bindings are invisible to the body, so a CTE
  named `t` in the outer query cannot capture the view's `FROM t`. The body's own `WITH` works
  normally.
- **No parameters.** `$N` cannot appear (`42P02` at CREATE).
- **The view's own database only.** Every relation the body names is a persistent relation of the
  database the view lives in (§10).

The body still runs inside the referencing statement. It observes the statement's snapshot, its
session settings (time zone, collation bundles, entropy and clock seams), its cost meter and
ceiling, and its query-memory account. A function with a side effect on statement state, such as
`nextval`, has that effect in the referencing statement, as it would in a derived table: a `SELECT`
from a view that calls `nextval` is a write ([sequences.md](sequences.md) §4).

A view may reference other views. Each nested reference expands to its own derived table. Cycles
cannot exist: a view can name only relations that already exist, and nothing in this slice
redefines a view (§12).

**Nesting depth.** A view body is parsed once, on its own, but every use plans it inside the
referencing statement, so the planner recurses through every view the statement reaches. The nesting
counts toward the parser's `MAX_EXPR_DEPTH` (`54001`, [cost.md](cost.md) §7) as it would in the
statement's **textual expansion**, the statement with every view reference replaced by its
parenthesized body (`FROM v` by `FROM ( <body> ) AS v`), recursively. A statement whose expansion
would not parse within the limit is `54001`; at `CREATE VIEW` the body planned is the new view's,
so a view whose expansion is too deep cannot be created.

The check needs no expansion text. When a stored definition is parsed, at `CREATE VIEW` and at open,
its **body depth** is recorded: the deepest nesting level the parse reached, counting the body's own
`subquery_expr` level. The parser also records the nesting depth at each base-table FROM item. A
body planned for a view reference starts at

```
base = enclosing base + depth of the reference within its own text
```

where the enclosing base is 0 for the statement and, inside a view body, that body's own base. The
reference is `54001` when `base + body depth > MAX_EXPR_DEPTH`. Parsing the derived table
`( <body> )` at the reference's depth reaches exactly that depth, so the check accepts a statement
exactly when its textual expansion parses. It is made at every view reference the planner meets,
including one a shape reads from the columns of a view already planned (planner.md §3.2), before the
expansion count below. Each nested view adds at least one level, so this also bounds how many views
nest inside one another.

**Expansion limit.** A view expands like a macro, so views that each name the previous view twice
expand to `2^d` view bodies at depth `d`. Planning is unmetered, so the expansion is bounded
structurally. One view reference written outside any view body (in the statement, or in the body a
`CREATE VIEW` defines) may expand to at most **`MAX_VIEW_EXPANSIONS = 1024`** views. The count
includes the view itself and every view reference reached through it, at any depth, each counted
once per reference. One more is **`54001`** (`statement_too_complex`, [cost.md](cost.md) §7e), also
at `CREATE VIEW`, whose body is planned. Two written references are counted separately, so
`FROM v9 a, v9 b` is within the limit when `v9` alone is. The count is a property of the statement
and the catalog. It does not depend on how often the planner plans a body
([planner.md](planner.md) §3.2): every view in the expansion is counted exactly once whatever is
pushed into it.

## 5. Freezing `*` at `CREATE VIEW`

PostgreSQL expands `*` when the view is defined, so a column later added to the base table does not
appear in the view. jed matches that by rewriting the stored text. While the body is planned at
`CREATE VIEW`, each `*` and `rel.*` select item that the planner expands is replaced, in the token
stream, by the explicit list it expanded to:

- a column of a relation that has a label renders as `"label" . "column"`;
- a column with no label renders as `"column"`. That covers an unaliased derived table's column and
  a `USING`/`NATURAL` merged column, which bare `"column"` resolves to.

Every substituted name is a **quoted identifier**, the rule for every engine-substituted name
([alter.md](alter.md) §2.2), so a column named `null` or `select` cannot re-lex as a keyword.
Example: `CREATE VIEW v AS SELECT * FROM t` over `t(a, b)` stores
`select "t" . "a" , "t" . "b" from t`.

This applies to every `*` anywhere in the body: in derived tables, in CTE bodies (a CTE the body
never references is still planned, so its `*` is frozen too), in set-operation arms, and in
`IN`/`EXISTS`/scalar subqueries. A composite whole-row `(expr).*` is a value expansion over a composite type's fixed
fields and is left as written.

**Verification.** The expanded text is parsed and planned again. It must produce the same output
column names and types as the original body. If it does not, `CREATE VIEW` fails with **`0A000`**.
This happens only when an expanded column cannot be referenced unambiguously by name, for example
`CREATE VIEW x (p, q) AS SELECT * FROM (SELECT 1 AS a, 2 AS a) d`, whose `*` covers two columns
named `a`. (Without the column list the duplicate output name is already `42701`, step 5 of §1.1.)
PostgreSQL accepts such a view by inventing aliases; jed records it as a narrowing (§12).

Output names are unchanged by expansion: `t.*` names its columns after the columns, and so does
`"t"."a"`.

## 6. Dependencies

A view **depends on** every table and view that a FROM item anywhere in its body binds to, at any
nesting level. That includes derived tables, CTE bodies, subqueries, and set-operation arms. It
also depends on **each column of a table that the body references**: through a column reference
(bare, qualified, or correlated within the body), a `USING`/`NATURAL` join column (both inputs'
columns), or a `*` expansion. A FROM item that binds to a CTE, a set-returning function, `VALUES`,
or a `jed_` catalog relation creates no dependency. A reference to a column of a view creates only
the dependency on that view.

The dependency list is computed by planning the body. It is **persisted** in the view's catalog
entry rather than recomputed. Dependency checks are then catalog lookups, and open never plans
(§3). `RENAME` keeps it current (§7). Re-validation (§8) recomputes it and requires that it is
unchanged.

The dependencies are enforced as follows, all **RESTRICT**:

| Operation | Effect | Code |
|---|---|---|
| `DROP TABLE t` while a view depends on `t` | refused | `2BP01` |
| `DROP VIEW v` while another view depends on `v` | refused | `2BP01` |
| `ALTER TABLE t DROP COLUMN c` while a view depends on `t.c` | refused | `2BP01` |
| `ALTER TABLE t ALTER COLUMN c TYPE …` while a view depends on `t.c` | refused (*cannot alter type of a column used by a view*) | `0A000` |
| `… CASCADE` of any of the above when a **view** is the dependent | refused (a follow-on, §12) | `0A000` |
| `ALTER TABLE t RENAME TO …` / `RENAME COLUMN …` | the view's text is rewritten (§7) | — |
| Any other `ALTER TABLE t` while a view depends on `t` | allowed, then re-validated (§8) | — |

The drop-set exclusion of `DROP TABLE` ([grammar.md](grammar.md) §13) applies to `DROP VIEW`: a
dependent that is itself dropped by the same statement does not block. `DROP VIEW v1, v2` succeeds
when `v2` depends on `v1`, in either order. A `DROP TABLE` statement cannot drop views, so a
dependent view always blocks it. `DROP TABLE … CASCADE` still drops dependent foreign keys, but
when a view depends on a table being dropped it raises `0A000` instead. `DROP VIEW … CASCADE` with
no dependent view behaves like `RESTRICT`. The check order is the same as for foreign keys: every
name is resolved (`42P01`/`42809`) before any dependency is checked.

The `2BP01` message names the first blocking view in name byte order:
*cannot drop table t because view v depends on it*, *cannot drop view v because view w depends on
it*, *cannot drop column c of table t because view v depends on it*.

**Not tracked.** A view does not record dependencies on sequences named in `nextval('s')`, on
composite types named in casts, on collations, or on host functions. Dropping such an object
succeeds, and the view then fails *when used* with the ordinary error (`42P01`, `42704`, `42883`).
PostgreSQL tracks some of these (sequences and types) and refuses the drop with `2BP01`. That is a
recorded divergence (§12). Because open never plans, the database still opens.

## 7. `RENAME` rewrites the stored text

Text binds by name, so a rename of something a view reads must rewrite the view. Each rewrite is a
**token edit** on the stored definition, which is then re-rendered canonically. Untouched tokens
keep their spelling. Every inserted or substituted name is a quoted identifier, and an inserted
keyword is the bare word `as`. The edits apply to each FROM item that **binds** the table, as
determined by planning the view body against the pre-statement catalog; a FROM item with the same
spelling that binds a CTE is never touched.

**`ALTER TABLE t RENAME TO t2`.** In every view that depends on `t`, the table-name token of each
FROM item that binds `t` is replaced by `"t2"`. If the item had no alias, `as "t"` is inserted after
the name, so the item's label, and every `t.col` qualifier in the body, is unchanged. A database
qualifier before the name (`main .`) is kept. `FROM t WHERE t.a > 1` becomes
`from "t2" as "t" where t . a > 1`, and `FROM t x` becomes `from "t2" x`. The view's dependency
entry is renamed.

**`ALTER TABLE t RENAME COLUMN c TO c2`.** The view keeps seeing the column as `c`, as in
PostgreSQL, through a **base-table column alias list** ([grammar.md](grammar.md) §15). Let `k` be
the renamed column's position. In every view that depends on `t` (not only the ones that read `c`),
each FROM item that binds `t` is edited so its column alias list covers positions `0 … k`:

1. If the item has no alias, `as "<label>"` is inserted after the table name, where the label is the
   name the item was already known by (the table's pre-statement name).
2. If it has no list, `( "n0" , … , "nk" )` is inserted after the alias. If it has a shorter list,
   entries are appended before its `)`. A list that already covers position `k` is unchanged.

Each entry `nj` is the name the item exposed for position `j` before the statement: the list's own
entry where there is one, otherwise the column's pre-statement name. Over `t(a, c, d)`, `FROM t`
becomes `from t as "t" ( "a" , "c" )`. The view's column dependency is renamed. Freezing every view
that reads `t`, not just those that read `c`, keeps a rename from capturing an unrelated reference:
a column renamed to `x` cannot make a view's bare `x` ambiguous or rebind it.

**`ALTER TABLE t DROP COLUMN c` (a column no view references).** Positions after `c` shift left. In
every view that depends on `t`, each FROM item that binds `t` and has a column alias list reaching
`c`'s position loses that entry and its separating comma. A one-entry list that loses its entry is
removed with its parentheses. The entry was not referenced, because no view depends on `t.c`.

**`ALTER TABLE t ADD COLUMN`** needs no rewrite. The new column is past every alias list. Whether it
captures a reference is checked by re-validation (§8).

The `RENAME` forms of `ALTER TABLE` (alter.md §2.1/§2.2) and `DROP COLUMN` (alter.md §3.2) gain
these rewrites. Their other behavior is unchanged. All view rewrites happen in the same
all-or-nothing statement as the catalog change.

## 8. Re-validation after `ALTER TABLE`

Name binding can change silently when a table changes. Suppose a view's subquery mentions `x`,
meaning the outer query's `u.x`. If `t` gains a column `x`, `x` now binds to `t.x` and the view's
meaning changes with no error. PostgreSQL is immune because its views are bound by object id. jed
closes the hole by re-validation:

> After every `ALTER TABLE t …` statement (any action, including the `RENAME` forms), each view
> that depends on `t` is planned again in the post-statement catalog. It must plan successfully,
> and its recomputed dependency list must equal its persisted dependency list (after the
> statement's own renames). Otherwise the `ALTER TABLE` fails with **`0A000`** (*cannot alter table
> t because it would change the definition of view v*) and nothing changes.

Equal dependencies mean every name in the body binds to the same columns, so the output types and
rows are unchanged. Only direct dependents are re-validated. A view that reads another view sees
the same output names and types, so it cannot be affected.

In practice this blocks an `ADD COLUMN` (or a rename of an unreferenced column into a new name)
only when the new name would capture or make ambiguous an unqualified reference in a view body.
PostgreSQL accepts those, and jed records the divergence (§12). Qualifying column references in
view bodies avoids it. The other `ALTER TABLE` actions cannot change a binding and always pass.
They are re-validated anyway because one uniform rule is simpler to keep correct than a list of
exempt actions.

Re-validation is DDL planning: it is unmetered and charges nothing.

## 9. Privileges

jed has no owners ([session.md](session.md) §5.3), so PostgreSQL's view-owner check is done once,
when the owner would be fixed:

- **`CREATE VIEW`** requires `allow_ddl` and, for the creating session, `SELECT` on every relation
  the body reads: each dependency (§6) and each `jed_` catalog relation the body names. A missing
  privilege is `42501`. A session therefore cannot create a view that reads something it could not
  read itself.
- **Reading a view** requires `SELECT` on the **view** only. The relations inside the body are not
  checked against the reading session. A host can therefore expose a subset of a table, some
  columns or some rows, to a session that has no grant on the table itself, which is PostgreSQL's
  default (non-`security_invoker`) behavior.
- **Function `EXECUTE`** for functions called in the body is checked against the reading session,
  as it would be in a derived table. PostgreSQL also checks function privileges against the
  invoker.
- **`DROP VIEW`** requires `allow_ddl`.

This keeps CLAUDE.md §13 intact. A view grants nothing its creator lacked. Reading through it is
bounded by the reading session's cost ceiling, lifetime budget, and query-memory limit like any
other query. The built-in surface stays pure.

## 10. Scope: temporary and attached databases

- **Temporary views** (`CREATE TEMP VIEW`, `CREATE VIEW temp.v`) are `0A000`. They are a
  temp-tables follow-on ([temp-tables.md](temp-tables.md) §14).
- **A persistent view may not read a temporary relation.** A body that names a temp table (bare or
  `temp.`-qualified) is `0A000`. PostgreSQL silently makes such a view temporary; jed has no
  temporary views yet (§12). Under the preclude-overlaps rule
  ([temp-tables.md](temp-tables.md) §3) a temp table cannot later shadow a name a view reads.
- **Attached databases.** `CREATE VIEW reports.v`, a body naming `reports.t`, and a reference to a
  view through an attachment qualifier (`FROM reports.v`) are all `0A000` in this slice. A file
  containing views can still be attached. Its views are unreachable through the attachment, but they
  are kept intact: because rewriting and re-validating a view (§7/§8) plans its body in its own
  database, any `ALTER TABLE reports.t …` is `0A000` while a view in `reports` depends on `t` (a
  follow-on). (`DROP TABLE` takes no database qualifier, so it cannot reach an attachment's table.)
  `main.`-qualified names work everywhere and are stored as written.

## 11. Introspection and the dump

**`jed_views`** is a new catalog relation ([introspection.md](introspection.md) §5.4). It is
computed, read-only, scoped by the database qualifier, gated like a table, and charges
`generated_row` per row:

```
jed_views(
  name        text   NOT NULL,  -- the view name (exact, grammar.md §3)
  definition  text   NOT NULL,  -- the stored canonical definition (§3), after * expansion and renames
  columns     text[] NOT NULL   -- the output column names, in order
)
```

Views are **not** listed in `jed_tables` or `jed_columns`, which describe stored tables. This is
PostgreSQL's `pg_tables` / `pg_views` split. A view's column types are not catalog facts (§3); a
client finds them by preparing `SELECT * FROM v`. Listing view columns in `jed_columns` is a
follow-on.

**The CLI dump** (`jed --dump`, [cli.md](cli.md)) emits each view as
`CREATE VIEW <name> (<columns>) AS <definition>;`, after every table, its rows, and its indexes, and
in dependency order among views (a view after every view it reads; ties in name byte order). The
output column list is always written, so the recreated view has the same column names even when the
body's own names differ.

## 12. Divergences and deferred work

Recorded divergences from PostgreSQL (CLAUDE.md §1):

| Divergence | Reason |
|---|---|
| Writes to every view are `55000`; PG updates simple single-table views | updatable views are a follow-on |
| `CREATE VIEW IF NOT EXISTS` is accepted; PG has no such syntax | SQLite parity; the `CREATE TABLE` precedent |
| A view whose `*` expansion is not referenceable by name is `0A000` | jed stores text; PG invents aliases in its deparser |
| `ALTER TABLE` that would capture or make ambiguous a view's reference is `0A000` | text binds by name (§8) |
| A view over a temp relation is `0A000`; PG makes it temporary | temporary views are a follow-on |
| Dropping a sequence or composite type a view uses succeeds; the view fails at use | those dependencies are not tracked (§6) |
| `ALTER TABLE v RENAME …` and `ALTER VIEW` are `0A000` | view renames are a follow-on |
| A zero-column view is `0A000` | the canonical text cannot express an empty select list after `*` expansion |
| `ANALYZE v` succeeds silently; PG also emits a warning | jed has no notice channel |
| `DROP SEQUENCE v` / `ALTER SEQUENCE v` are `42P01`; PG says `42809` | sequence statements already treat a table this way |
| A reference that expands to more than 1024 views is `54001`; PG has no limit | planning is unmetered, so it is bounded structurally (CLAUDE.md §13; §4.1) |

Deferred follow-ons, none foreclosed:

- `CREATE OR REPLACE VIEW`, `ALTER VIEW`, view renames (`0A000` today);
- `WITH [CASCADED | LOCAL] CHECK OPTION` and `WITH (options)` (`0A000`);
- updatable views and `INSTEAD OF` behavior;
- `CREATE MATERIALIZED VIEW` and `CREATE RECURSIVE VIEW` (`0A000`);
- temporary views and views in or over attached databases (`0A000`), including `ALTER TABLE` of an
  attached table that one of its database's views depends on;
- `DROP … CASCADE` that drops dependent views (`0A000`);
- dependency tracking for sequences, composite types, collations, and host functions;
- view columns in `jed_columns`;
- definitions longer than one catalog page (`0A000`), which would need an overflow chain.

## 13. Determinism and the cross-core contract

- The stored text is a byte contract: the same `CREATE VIEW` stores the same bytes in every core
  (format.md *Check-expression text*, plus the substitution rules of §5 and §7).
- The dependency list is sorted (§3, format.md) and computed from the resolved body, never from a
  map's iteration order.
- Reading a view is the equivalent derived table, which is already deterministic in rows, cost,
  plan, and errors.
- The `2BP01` and `0A000` messages name the first blocking view in name byte order.

## 14. Slices

1. **V1 — create, read, drop.** Grammar; the catalog entry (`format_version` 38) and a golden
   fixture; the isolated scope and derived-table reading; `*` freezing; dependencies with
   `DROP TABLE`/`DROP VIEW`/`DROP COLUMN`/`ALTER COLUMN TYPE` enforcement; re-validation after
   `ALTER TABLE`; privileges; `55000` on writes; the `42809` matrix; `jed_views`; the CLI dump;
   the NoREC relation. Until V2/V4 land, a `RENAME` that breaks a view fails re-validation
   (`0A000`).
2. **V2 — `RENAME TO` rewrite** (§7).
3. **V3 — base-table column alias lists** (`FROM t AS x(a, b)`, [grammar.md](grammar.md) §15), a
   PostgreSQL feature in its own right and the mechanism V4 needs.
4. **V4 — `RENAME COLUMN` and `DROP COLUMN` rewrites** (§7).

Capabilities: `ddl.view` (V1), `ddl.view_rename` (V2 + V4), `query.table_column_alias` (V3),
`introspect.views` (V1).
