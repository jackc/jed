# Views — design

> `CREATE [OR REPLACE] VIEW name [(cols)] AS query` / `DROP VIEW` / `ALTER VIEW … RENAME`: a
> **named, persisted query** that the FROM clause references like a table. This doc is the contract all three cores implement in lockstep
> (CLAUDE.md §2). The grammar is in [../grammar/grammar.ebnf](../grammar/grammar.ebnf) and
> [grammar.md](grammar.md) §54. The byte layout is in [../fileformat/format.md](../fileformat/format.md)
> (`format_version` 38). The errors are existing codes in [../errors/registry.toml](../errors/registry.toml).
> PostgreSQL behavior was pinned against the live `postgres:18` oracle (CLAUDE.md §1).
> A view with PostgreSQL's simple single-relation shape is **automatically updatable**: `INSERT`,
> `UPDATE`, and `DELETE` on it write its base relation (§15). There is no `CHECK OPTION`, and no
> materialized, recursive, or temporary view (§12). `CREATE OR REPLACE VIEW` (§1.2) and view renames
> (§7.1) redefine a view in place.

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
CREATE [OR REPLACE] VIEW [IF NOT EXISTS] [main.]name [( col [, …] )] AS query
DROP VIEW [IF EXISTS] name [, …] [CASCADE | RESTRICT]
ALTER VIEW [IF EXISTS] [main.]name RENAME TO new_name
ALTER VIEW [IF EXISTS] [main.]name RENAME [COLUMN] col TO new_col
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
  qualifier validation, and the `jed_` reservation still run first. `OR REPLACE` and `IF NOT
  EXISTS` together are **`42601`**: the two disagree about a taken name, and PostgreSQL has neither
  combination.
- **`OR REPLACE`** redefines an existing view in place (§1.2).
- **`ALTER VIEW … RENAME`** renames a view or one of its output columns (§7.1).
- **`DROP VIEW`** follows `DROP TABLE`'s shape ([grammar.md](grammar.md) §13). It is two-phase and
  all-or-nothing over a comma list. A repeated name is deduplicated. A missing name is **`42P01`**
  (*view does not exist*) unless `IF EXISTS`, which skips just that name. A name that resolves to
  a table, index, or sequence is **`42809`** (*"t" is not a view*), and `IF EXISTS` does not
  suppress it.
- **Transactional.** `CREATE [OR REPLACE] VIEW`, `DROP VIEW`, and `ALTER VIEW` are DDL in the §3
  staging model. They roll back
  with their transaction, are refused in a read-only transaction (`25006`), and are gated by the
  session's `allow_ddl` (`42501`, [session.md](session.md) §5).
- **Cost is zero** for every view statement. Planning is unmetered, and the statements read no rows
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
   exists. Under `OR REPLACE`, a name taken by a view continues at §1.2 step 9, and a name taken by
   any other relation is **`42809`** (*"t" is not a view*).
9. The catalog entry is written. A definition whose entry does not fit one catalog page is
   `0A000` (the catalog's one-page-per-entry rule, format.md *Catalog*).

### 1.2 `CREATE OR REPLACE VIEW` (PG-matched)

`CREATE OR REPLACE VIEW v …` where no relation `v` exists is exactly `CREATE VIEW v …`. Where `v` is
a view, steps 1–7 of §1.1 run unchanged (body errors, the column list, `*` freezing, and the
privilege check all come first, as in PostgreSQL), and then:

9. **Column compatibility** (`42P16`, *invalid_table_definition*), in PostgreSQL's order. Fewer
   output columns than the view has is *cannot drop columns from view*. Then each of the view's
   existing columns is compared, left to right, with the new column at its position: a different
   name is *cannot change name of view column "a" to "b"*, then a different type is *cannot change
   data type of view column "a" from i32 to i64*, then a different collation is *cannot change
   collation of view column "b" from "C" to "es"*. New columns may be **appended** after the
   existing ones. Names are the final output names (the column list applied over the body's names).
   A type is the resolved output type, without typmods (§12). A collation is the collation name the
   column's derivation carries ([collation.md](collation.md) §1); a column that derives no collation
   (a literal) has the database's default collation, PostgreSQL's `default`; an indeterminate
   collation is a value of its own.
   The existing columns' types and collations are not persisted (§3); they come from planning the
   view's current body against the current catalog. If that body no longer plans (an untracked
   dependency was dropped, §6), only the names and the column count are compared, so a broken view
   can always be repaired by replacing it.
10. **No cycle** (`42P17`, *infinite recursion detected in rules for relation "v"*). The new body is
    planned against the view's current definition, so it may read `v` itself or a view that reads
    `v`. A body that depends on `v`, directly or through other views, would make the catalog cyclic,
    which open rejects (§3). PostgreSQL accepts such a definition and fails each later use with this
    error; jed fails the definition (§12).
11. **Dependent views are re-validated** (§8) against the catalog with the replacement installed.
    A failure is **`0A000`** (*cannot replace view v because it would change the definition of view
    w*).
12. The catalog entry is replaced: the new definition, output columns, dependency list, and depth,
    under the same name. A definition that does not fit one catalog page is `0A000`, as at create.

The replacement changes nothing else. Dependent views keep their stored text, which binds `v` and
its existing columns by name, so they read the new definition with the same output names and
types. This is PostgreSQL's observable behavior.
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
| `INSERT INTO v` | writes the base relation if `v` is updatable (§15), else `55000` *cannot insert into view "v"* | same |
| `UPDATE v` | writes the base relation if `v` is updatable, else `55000` *cannot update view "v"* | same |
| `DELETE FROM v` | writes the base relation if `v` is updatable, else `55000` *cannot delete from view "v"* | same |
| `INSERT INTO v … ON CONFLICT` | `55000` if `v` is not updatable, else `0A000` (§15.2) | PG supports it on an updatable view |
| A writable CTE targeting `v` | as the statement it holds | same |
| `CREATE INDEX … ON v` | `42809` *cannot create index on relation "v"* | same |
| `REFERENCES v` (a foreign-key parent) | `42809` *referenced relation "v" is not a table* | same |
| `ALTER TABLE v RENAME TO …` / `RENAME [COLUMN] …` | the view rename (§7.1) | same |
| Any other `ALTER TABLE v …` action | `42809` | same |
| `ALTER VIEW t RENAME …` (`t` a table, index, or sequence) | `42809` *"t" is not a view*, also with `IF EXISTS` | PG renames a table's column through `ALTER VIEW` |
| `CREATE OR REPLACE VIEW t …` (`t` not a view) | `42809` *"t" is not a view* | same |
| `ANALYZE v` | a no-op success | PG warns and succeeds |
| `DROP VIEW t` (`t` a table, index, or sequence) | `42809` *"t" is not a view* | same |

`55000` (`object_not_in_prerequisite_state`) is PostgreSQL's code for writing to a view that is
not automatically updatable (§15.1). The rejection happens when the DML target is resolved, before
any column, value, or `RETURNING` check (§15.3).

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
cannot exist: a view can name only relations that already exist, `CREATE OR REPLACE VIEW` refuses a
definition that would read the view itself (`42P17`, §1.2), and a rename rewrites every reference
consistently (§7.1).

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

The dependencies are enforced as follows:

| Operation | Effect | Code |
|---|---|---|
| `DROP TABLE t` while a view depends on `t` | refused | `2BP01` |
| `DROP VIEW v` while another view depends on `v` | refused | `2BP01` |
| `ALTER TABLE t DROP COLUMN c` while a view depends on `t.c` | refused | `2BP01` |
| `ALTER TABLE t ALTER COLUMN c TYPE …` while a view depends on `t.c` | refused (*cannot alter type of a column used by a view*) | `0A000` |
| `DROP TABLE t CASCADE` / `DROP VIEW v CASCADE` / `ALTER TABLE t DROP COLUMN c CASCADE` | the dependent views are dropped too (§6.1) | — |
| `ALTER TABLE t RENAME TO …` / `RENAME COLUMN …` | the view's text is rewritten (§7) | — |
| Any other `ALTER TABLE t` while a view depends on `t` | allowed, then re-validated (§8) | — |
| `ALTER VIEW u RENAME TO …` / `RENAME COLUMN …` while a view depends on `u` | the view's text is rewritten (§7.1) | — |
| `CREATE OR REPLACE VIEW u` while a view depends on `u` | allowed, then re-validated (§8) | — |

The drop-set exclusion of `DROP TABLE` ([grammar.md](grammar.md) §13) applies to `DROP VIEW`: a
dependent that is itself dropped by the same statement does not block. `DROP VIEW v1, v2` succeeds
when `v2` depends on `v1`, in either order. A `DROP TABLE` statement cannot drop views, so a
dependent view blocks it under `RESTRICT`. The check order is the same as for foreign keys: every
name is resolved (`42P01`/`42809`) before any dependency is checked.

The `2BP01` message names the first blocking view in name byte order:
*cannot drop table t because view v depends on it*, *cannot drop view v because view w depends on
it*, *cannot drop column c of table t because view v depends on it*.

### 6.1 `CASCADE`

Under `CASCADE`, a dependent view is dropped instead of blocking the statement, as in PostgreSQL. The
views dropped are the **closure**: every view that depends on a dropped table (`DROP TABLE`), on a
dropped view (`DROP VIEW`), or on a dropped column (`ALTER TABLE … DROP COLUMN`), and then every view
that depends on one of those, repeated until no view is added. A view that depends on none of them
is untouched. A view dependency is on the whole view, so a view that reads any column of a dropped
view is dropped.

- **`DROP TABLE t [, …] CASCADE`** drops the closure of the views depending on any listed table,
  together with the foreign keys `CASCADE` already drops (grammar.md §13).
- **`DROP VIEW v [, …] CASCADE`** drops the listed views and their closure. A view in both is
  dropped once.
- **`ALTER TABLE t DROP COLUMN c CASCADE`** drops the closure of the views whose dependency list
  names `t.c` (§6: a column reference, a `USING`/`NATURAL` column, or a frozen `*`). A view that
  depends on `t` but not on `c` survives, and is rewritten (§7) and re-validated (§8) as for any
  `DROP COLUMN`.

Within one `ALTER TABLE`, the actions run in order, so a view an earlier `DROP COLUMN … CASCADE`
dropped no longer blocks a later `DROP COLUMN` under `RESTRICT`, while a `RESTRICT` drop written
before the `CASCADE` one still fails with `2BP01`. The `ALTER COLUMN … TYPE` view check runs after
every action, because PostgreSQL executes all drops before any type change: `ALTER COLUMN g TYPE
i64, DROP COLUMN f CASCADE` succeeds when the only view using `g` also uses `f`.

Dropping views is a catalog edit with zero cost, like the drop itself. PostgreSQL also emits a
*drop cascades to* notice; jed has no notice channel (§12). The statement is all-or-nothing: a
later failure, or a rollback of its transaction, restores every view it dropped.

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

### 7.1 Renaming a view

```
ALTER VIEW [IF EXISTS] [main.]v RENAME TO v2
ALTER VIEW [IF EXISTS] [main.]v RENAME [COLUMN] a TO b
```

`ALTER TABLE v RENAME TO …` and `ALTER TABLE v RENAME [COLUMN] …` on a view are the same
statements, as in PostgreSQL. Validation, in order:

1. The `allow_ddl` gate (`42501`), then the qualifier: `main` is accepted, `temp` and any
   attachment are `0A000` (§10), and an unknown qualifier is `42P01`.
2. The name. No relation `v` is **`42P01`** (*relation "v" does not exist*), or a silent no-op
   under `IF EXISTS`. Under `ALTER VIEW`, a table, index, or sequence `v` is **`42809`** (*"v" is
   not a view*), and `IF EXISTS` does not suppress it.
3. `RENAME TO`: the `jed_` reservation (`42939`, kind `view`), then a taken name, including `v`
   itself, is **`42P07`** (*relation "v2" already exists*).
   `RENAME COLUMN`: a column `a` the view does not have is **`42703`** (*column "a" does not
   exist*), then a column `b` the view already has, including `a` itself, is **`42701`** (*column "b"
   of relation "v" already exists*). Names are exact (grammar.md §3), so `RENAME COLUMN A TO c`
   renames `a`.
4. The rename, and the rewrite of each view that depends on `v`, below.
5. Re-validation of those dependent views (§8). A failure is **`0A000`** (*cannot alter view v
   because it would change the definition of view w*).

**`RENAME TO`** changes the view's catalog key and nothing in its own entry. Each view that depends
on `v` is rewritten exactly as for a table rename (§7): the name token of each FROM item that binds
`v` becomes `"v2"`, with `as "v"` inserted when the item has no alias, and its dependency entry is
renamed.

**`RENAME COLUMN`** changes the view's stored output column list; the definition is unchanged. Each
view that depends on `v` keeps seeing the column as `a`, exactly as for a table column rename (§7):
each FROM item that binds `v` gets a column alias list covering positions `0 … k` with the names it
exposed before the statement. Over `v(a, b)`, a dependent's `FROM v` becomes
`from v as "v" ( "a" )` when `a` is renamed. This is PostgreSQL's behavior: its dependents keep the
names they were defined with (`pg_get_viewdef` shows `aa AS a`).

Other `ALTER VIEW` actions (`ALTER COLUMN … SET/DROP DEFAULT`, `OWNER TO`, `SET SCHEMA`, `SET` /
`RESET` options) are **`0A000`** (§12). Statements that rename a view are zero-cost, transactional
DDL like the others (§1).

## 8. Re-validation after a change to a dependency

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

The same rule applies after `ALTER VIEW u RENAME …` (§7.1) to each view that depends on `u`, with
the *alter view* message.

**After `CREATE OR REPLACE VIEW u`** (§1.2) the rule is stronger, because the replacement can add
columns to `u` and changes its body. Every view that reads `u`, directly or through other views, is
planned again in name byte order with the replacement installed, and must plan successfully. That
catches a replacement whose deeper body or wider expansion would push a reader past the nesting or
expansion limit (§4.1). Each **direct** dependent must also keep its persisted dependency list and
must not reference a column that the replacement appended. Which columns of `u` a body references
is observed while it is planned, as for table columns (§6), but it is not persisted. A reference to
an appended column can only be a capture: a name that bound elsewhere before the replacement, such
as a correlated reference to an outer relation's `x`, now binds to `u`'s new `x`. The first failing
view in name byte order is named in the `0A000` (*cannot replace view u because it would change the
definition of view w*). The view's existing columns keep their names and types (§1.2 step 9), so
no other binding can change.

In practice this blocks an `ADD COLUMN` (or a rename of an unreferenced column into a new name)
only when the new name would capture or make ambiguous an unqualified reference in a view body.
PostgreSQL accepts those, and jed records the divergence (§12). Qualifying column references in
view bodies avoids it. The other `ALTER TABLE` actions cannot change a binding and always pass.
They are re-validated anyway because one uniform rule is simpler to keep correct than a list of
exempt actions.

Re-validation is DDL planning: it is unmetered and charges nothing. PostgreSQL binds views by object
id and accepts every one of these changes; each refusal is a recorded divergence (§12).

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
- **`CREATE OR REPLACE VIEW`** has `CREATE VIEW`'s requirements, checked against the new body.
- **`DROP VIEW`** and **`ALTER VIEW`** require `allow_ddl`.
- **Writing through a view** (§15.4) requires the writer's privilege on the view only, as for a
  table.

This keeps CLAUDE.md §13 intact. A view grants no read its creator lacked. Reading through it is
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
  view through an attachment qualifier (`FROM reports.v`, or a write target `INSERT INTO reports.v`)
  are all `0A000` in this slice. A file
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
| A write to a view that is not updatable is `55000` before any column or expression error; PG reports its parse-analysis errors (`42703`, `42601`, a bad literal) first | updatability is checked when the target resolves (§15.3) |
| Assigning a read-only view column is `0A000` before the statement's expressions are resolved; PG reports it after them | the check belongs to the target columns (§15.3) |
| Two view columns that reach the same base column, assigned together, are `42701`; PG says `42601` | jed's duplicate-assignment code for tables (`UPDATE t SET a = 1, a = 2`) |
| `INSERT … ON CONFLICT` on an updatable view is `0A000`; PG supports it | a follow-on (§15.2) |
| A write through a view checks only the writer's privilege on the view; PG also checks the view owner's privilege on the base relation | jed has no persistent owners, and the session that creates a view already holds `allow_ddl`, which is trusted schema authority (§15.4) |
| `CREATE VIEW IF NOT EXISTS` is accepted; PG has no such syntax | SQLite parity; the `CREATE TABLE` precedent |
| A view whose `*` expansion is not referenceable by name is `0A000` | jed stores text; PG invents aliases in its deparser |
| `ALTER TABLE` that would capture or make ambiguous a view's reference is `0A000` | text binds by name (§8) |
| `CREATE OR REPLACE VIEW` whose appended column would capture or make ambiguous a dependent view's reference is `0A000` | text binds by name (§8) |
| `CREATE OR REPLACE VIEW` of a view that would read itself, directly or through other views, is `42P17`; PG accepts it and fails every use with `42P17` | the catalog stays acyclic, so open can check it (§3) |
| `CREATE OR REPLACE VIEW` compares column types without typmods: `varchar(3)` → `varchar(4)` or `text`, and `decimal(5,2)` → `decimal(6,2)`, are accepted; PG refuses them (`42P16`) | jed's query output columns carry no typmod |
| A bare integer literal cannot replace an `i32` view column (`0 AS d`); PG accepts it | a projected integer literal is `i64` in jed ([types.md](types.md) §6) |
| `CREATE OR REPLACE VIEW` compares collation names: a column of the default collation replaced by the same collation named explicitly is accepted; PG refuses (`default` and `"C"` are different collation objects there) | jed has no separate default-collation object |
| `ALTER VIEW t RENAME COLUMN …` on a table is `42809`; PG renames the table's column | `ALTER VIEW` names views only, as its `RENAME TO` already does in PG |
| `ALTER VIEW` actions other than `RENAME` are `0A000` | defaults, owners, schemas, and view options are follow-ons or absent |
| A view over a temp relation is `0A000`; PG makes it temporary | temporary views are a follow-on |
| Dropping a sequence or composite type a view uses succeeds; the view fails at use | those dependencies are not tracked (§6) |
| A zero-column view is `0A000` | the canonical text cannot express an empty select list after `*` expansion |
| `ANALYZE v` succeeds silently; PG also emits a warning | jed has no notice channel |
| `DROP … CASCADE` drops dependent views silently; PG also emits a *drop cascades to* notice | jed has no notice channel |
| `DROP SEQUENCE v` / `ALTER SEQUENCE v` are `42P01`; PG says `42809` | sequence statements already treat a table this way |
| A reference that expands to more than 1024 views is `54001`; PG has no limit | planning is unmetered, so it is bounded structurally (CLAUDE.md §13; §4.1) |

Deferred follow-ons, none foreclosed:

- the other `ALTER VIEW` actions: column defaults (`UPDATE v SET c = DEFAULT` would then assign
  them, §15.2), `SET` / `RESET` view options;
- `WITH [CASCADED | LOCAL] CHECK OPTION` and `WITH (options)` (`0A000`);
- `INSERT … ON CONFLICT` on an updatable view (`0A000`, §15.2);
- `INSTEAD OF` behavior for views that are not automatically updatable (jed has no triggers or
  rules);
- an updatability column in `jed_views` (PostgreSQL's `information_schema.views.is_updatable`);
- `CREATE MATERIALIZED VIEW` and `CREATE RECURSIVE VIEW` (`0A000`);
- temporary views and views in or over attached databases (`0A000`), including `ALTER TABLE` of an
  attached table that one of its database's views depends on;
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
- A `CASCADE` closure (§6.1) is a set computed from the persisted dependency lists, so which views
  it drops never depends on iteration order.
- `CREATE OR REPLACE VIEW` compares columns in position order and reports the first difference in
  PostgreSQL's order (count, then per column name, type, collation).
- A write through a view is its equivalent base statement on the base table (§15.2, §15.5), which is
  deterministic in rows, end state, cost, plan, and errors. The `0A000` for a read-only column names the first one in the
  view's column order, and the view-level checks run from the target view down (§15.3).

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

5. **V5 — `CREATE OR REPLACE VIEW`** (§1.2): column compatibility, the cycle check, and
   re-validation of the views that read the replaced view (§8).
6. **V6 — view renames** (§7.1): `ALTER VIEW … RENAME TO` / `RENAME [COLUMN]`, and the same
   `ALTER TABLE` forms on a view, with the §7 rewrites of dependent views.
7. **V7 — automatically updatable views** (§15): `INSERT` / `UPDATE` / `DELETE` through a view,
   chains of views, `RETURNING`, writable CTEs, privileges checked on the view as for a table, and
   the view-vs-base-statement NoREC relation.
8. **V8 — `DROP … CASCADE` of dependent views** (§6.1): `DROP TABLE`, `DROP VIEW`, and `ALTER TABLE
   … DROP COLUMN` drop the closure of the views that depend on what they drop.

Capabilities: `ddl.view` (V1), `ddl.view_rename` (V2 + V4), `query.table_column_alias` (V3),
`introspect.views` (V1), `ddl.view_replace` (V5), `ddl.view_alter` (V6), `dml.view_write` (V7),
`ddl.view_cascade` (V8).

## 15. Writing through a view

A view with the simple shape of §15.1 is **automatically updatable**, as in PostgreSQL: an `INSERT`,
`UPDATE`, or `DELETE` that names it writes the relation its body reads. Every other view refuses
writes with `55000`. Behavior below was pinned against the PostgreSQL 18 oracle; the divergences are
in §12.

### 15.1 Updatable views and columns

A view is **updatable** when its stored body (§3) is a single `SELECT`, not a set operation, with:

- no `WITH` clause;
- no `DISTINCT`, `GROUP BY`, `HAVING`, `LIMIT`, or `OFFSET`;
- no aggregate or window function at the `SELECT`'s own level (one inside a subquery does not
  count), and no set-returning function in its select list;
- exactly one `FROM` item, and that item is a table or a view, with or without an alias and a column
  alias list. A join, a derived table, a CTE reference, a set-returning function, `VALUES`, and a
  `jed_` catalog relation are not.

The body may have a `WHERE`, including subqueries over any relation, and an `ORDER BY`, which a
write ignores. The table or view the `FROM` item names is the view's **base relation**.

A view column is **updatable** when its select item is a reference to a column of the `FROM` item,
bare or qualified, optionally parenthesized. That column is the view column's **base column**. Every
other select item (an expression, a cast, a literal, a function call, a subquery, `COLLATE`, a
composite field selection) makes a **read-only** column. A read-only column can be read anywhere in
a write but cannot be assigned. Two view columns may share a base column (`SELECT a, a AS a2`).

For `INSERT` and `UPDATE` a view also needs at least one updatable column. `DELETE` does not, so
`DELETE FROM v` works on `CREATE VIEW v AS SELECT a + 1 AS x FROM t`, but `INSERT` and `UPDATE` on it
are `55000`.

Updatability is a property of one view's body. When the base relation is itself a view, whether
that view is updatable is checked when the write reaches it (§15.3). A write through a chain of
views writes the table at the bottom of the chain.

### 15.2 What a write means

A write through an updatable view `v` with base relation `b` is a write to `b`, called the
**equivalent base statement**. Its parts are defined as follows.

**Names.** The statement's target is the view. It exposes `v`'s output columns under the view's
name, exactly as a table target exposes its columns under the table's. A column of `b` that `v`
does not output is not visible: `UPDATE v SET c = 1` is `42703` (*column "c" of
relation "v" does not exist*) even when `b` has a column `c`.

**View column references.** A reference to a column of `v` anywhere in the statement (a `SET`
source, the `WHERE`, `RETURNING`, or a subquery inside any of them) denotes that column's select
item evaluated on the base row the statement is processing. The select item keeps the body's
bindings (§4.1): its names resolve in the view's isolated scope against the body's `FROM` item, with
the body's label and column alias list, and never against the statement's relations or CTEs. A
reference to an updatable column is therefore the base column itself. A reference to a read-only
column evaluates its select item each time, exactly as if the expression were written in its place.

**`DELETE FROM v WHERE p`** is `DELETE FROM b WHERE w AND p`, where `w` is `v`'s `WHERE` (just `p`
when `v` has none, just `w` when the statement has none). A `WHERE` is a filter, so its top-level
`AND` chain evaluates left to right and stops at the first conjunct that is not TRUE
([cost.md](cost.md) §3): `p` is evaluated only on the rows `w` keeps, and a row the view does not show — `w` FALSE or
NULL — can never raise an error from `p`. This is the order the two conditions apply when the view
is read (§4), where `w` filters the derived table's body. (PostgreSQL's rewriter likewise adds the
view's qualification to the statement's `WHERE`.)

**`UPDATE v SET c = e, … WHERE p`** is `UPDATE b … WHERE w AND p`, with `w` as for `DELETE`. Each
assignment to view column `c` assigns `c`'s base column the value of `e`. `SET c = DEFAULT` assigns the view column's own default, as in PostgreSQL. jed has no view column
defaults (§12), so that is NULL, not the base column's default; on a `GENERATED ALWAYS` identity
base column it is therefore `428C9`. An update may change a row so that `w` no longer holds; the
row stays updated and leaves the view.

**`INSERT INTO v [(c, …)] source`** inserts into `b`. The target view columns are the listed ones,
or, without a list, every column of `v` in order. The source supplies exactly one value per target,
as for a table ([grammar.md](grammar.md) §12; PostgreSQL also accepts fewer values without a list, a
recorded narrowing for tables too), so a list-less `INSERT` into a view with a read-only column is
`0A000`. Each target assigns its base column. Every column of `b` that no target assigns takes its
own default, else NULL, exactly like an omitted column of a direct `INSERT INTO b`. That covers the columns `v`
does not output, the view columns the statement omits, and a `DEFAULT` slot. `DEFAULT VALUES`
assigns no column. `OVERRIDING SYSTEM VALUE` and `OVERRIDING USER VALUE` apply to `b`'s identity
columns. `w` is not checked, so an inserted row may be one the view does not show. (`WITH CHECK
OPTION` is a follow-on, §12.)

**`RETURNING`** projects `v`'s columns over each affected base row: the stored row for `INSERT`, the
new row for `UPDATE`, the deleted row for `DELETE`. `*` expands to `v`'s output columns, and
`old.c` / `new.c` are view column `c` of the row's two versions. A subquery in a read-only column's
select item observes the pre-statement snapshot, like any `RETURNING` subquery.

**Chains.** When `b` is a view, the equivalent base statement is itself a write through `b`, so the
rule applies again until it reaches a table. The views' `WHERE` clauses are AND-ed left-deep, the
view nearest the table first, then the statement's `p`: `(w₁ AND w₂) AND p`.

**Everything else** is the base statement's own behavior: two-phase validation, `NOT NULL`, `CHECK`,
`UNIQUE`, primary-key re-keying, foreign keys, identity and serial columns, collated keys, the
transaction and access-mode checks (`25006`), and error messages, which name `b` and its columns. A
writable CTE may target an updatable view, with the same meaning.

**`ON CONFLICT`.** `INSERT INTO v … ON CONFLICT` is `0A000` (*ON CONFLICT on a view is not supported
yet*) when `v` is updatable, and `55000` when it is not. PostgreSQL supports it; it is a follow-on
(§12).

### 15.3 Validation order

1. The checks every DML target gets, unchanged: the writing session's privileges (`42501`,
   session.md §5.3), the catalog-relation check (`42809`), a read-only attachment (`25006`), and the
   database qualifier.
2. The target view is updatable for the statement's kind (§15.1), else **`55000`** (*cannot insert
   into view "v"*, *cannot update view "v"*, *cannot delete from view "v"*).
3. `ON CONFLICT` is `0A000` (§15.2).
4. The view is planned as a read of it at the statement's top level would plan it (§4, §4.1): an
   error in its body or in a view it reads, or the nesting and expansion limits (`54001`).
5. The statement's target columns resolve against `v`'s output columns, with the rules and codes of
   a table target: an unknown column is `42703`, and a column listed twice in `INSERT` or assigned
   twice in `UPDATE` is `42701`. (An `INSERT` source whose arity differs from its targets is `42601`
   where the table path checks it, which is later.)
6. For each view from `v` down the chain:
   1. a view after the first must be updatable for the statement's kind (`55000`, naming it);
   2. for `INSERT` and `UPDATE`, every target column of the view must be updatable, else **`0A000`**
      (*cannot insert into column "c" of view "v"*, *cannot update column "c" of view "v"*), naming
      the first read-only target in the view's column order. A `DEFAULT` slot or `SET c = DEFAULT`
      still targets `c`.

   The targets then map to the base relation's columns.
7. Two targets that reach the same column of the table are `42701` (*column "a" assigned more than
   once*).
8. The equivalent base statement's own validation follows in its existing order: identity
   (`428C9`), the `SET`, `VALUES`, `WHERE`, and `RETURNING` expressions, types, and then execution.

### 15.4 Privileges

A view is checked like a table. The writing session needs, on the view, the operation's privilege,
and `SELECT` when the statement reads the view's columns (in a `WHERE`, a `SET` source, or
`RETURNING`): the table rule of [session.md](session.md) §5.3 applied to the view. Nothing else is
checked against the writer. Neither the base relation nor, in a chain, the views below the target
are checked, just as reading a view needs `SELECT` on that view only (§9). A function a select item
or the view's `WHERE` calls needs the writer's `EXECUTE`, as when the view is read.

A host can therefore let a session write a subset of a table without any grant on the table: grant
`INSERT`, `UPDATE`, or `DELETE` on an updatable view over it.

PostgreSQL also checks the base relation, against the view's owner. jed has no persistent owners
(§9), and it does not record the creating session's write privileges either: the creating session
holds `allow_ddl`, which is trusted schema authority (it can already drop or rewrite the table), so a
recorded check would add persistent state while protecting little. A session with `allow_ddl` can
therefore write a table through a view it creates even when it lacks the privilege on the table. A
host that must prevent that withholds `allow_ddl`.

### 15.5 Plan, cost, and EXPLAIN

A write through a view is planned and executed as its equivalent base statement (§15.2), with no
charge for the view: the same access path, estimates, contradiction detection, `EXPLAIN`, rows,
errors, and **cost** as the hand-written statement on the base table. So `UPDATE v SET b = 1 WHERE
id = 5` on a view that outputs the primary key `id` is a point lookup, and `EXPLAIN` renders exactly
the plan of `UPDATE t SET b = 1 WHERE w AND id = 5`, its root naming the table (`Update t`). Each
`AND` node of the `WHERE` charges its `operator_eval` like any other.

The NoREC generator pins the rows with a **view-write-vs-base-statement** relation: the same write
through a view and written by hand against the base table, with the view's conditions inlined, must
return the same `RETURNING` rows and leave the same table contents ([conformance.md](conformance.md)
§8). The cost equality is pinned by `dml/view_write_jed.test`.
