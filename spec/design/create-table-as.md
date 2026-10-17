# CREATE TABLE … AS — design

> `CREATE [TEMP | TEMPORARY] TABLE [IF NOT EXISTS] [db.]name [( col [, …] )] [ON COMMIT …] AS query
> [WITH [NO] DATA]`: PostgreSQL's `CREATE TABLE AS`, a table whose columns are a query's output
> columns and whose rows are the query's rows. The grammar is authoritative for the surface
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) — `create_table`, `create_table_as`;
> [grammar.md](grammar.md) §57). PostgreSQL 18 behavior (`src/backend/commands/createas.c`,
> `exprTypmod` in `nodeFuncs.c`) was pinned against the live oracle (CLAUDE.md §1); the recorded
> divergences are in §5.
>
> **Status: landed (all three cores)**, with its follow-ons: a data-modifying `WITH` in the query
> (§1), `SELECT … INTO` (§6), and `EXPLAIN [ANALYZE] CREATE TABLE … AS` (§7). Corpus:
> `ddl/create_table_as.test` (PostgreSQL-checked), `ddl/create_table_as_types.test` (the created
> types, through `jed_columns`), `ddl/create_table_as_temp.test` (temp, `ON COMMIT`, atomicity, the
> session envelope), `ddl/create_table_as_writable_with.test` and `ddl/select_into.test`
> (PostgreSQL-checked), `ddl/select_into_jed.test`, `ddl/explain_create_table_as.test`,
> `attach/create_table_as.test`, `collation/create_table_as.test`, and
> `dml/insert_select_container.test` (the container `INSERT … SELECT` the fill relies on, §3).
> Capabilities `ddl.create_table_as`, `ddl.create_table_as_writable_with`, `ddl.select_into`, and
> `ddl.explain_create_table_as`. No `format_version` change: the created table is an ordinary table.

## 1. Surface

```
CREATE [ TEMP | TEMPORARY ] TABLE [ IF NOT EXISTS ] [ db . ] name [ ( column_name [, ...] ) ]
    [ ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP } ]
    AS query
    [ WITH [ NO ] DATA ]
```

- **`query`** is a `subquery_expr`: a `SELECT`, a `VALUES` query, a set operation, a parenthesized
  query, an `ORDER BY` / `LIMIT` / `OFFSET`, and a nested `WITH` (including `WITH RECURSIVE`) — what
  a derived table or a view body accepts ([grammar.md](grammar.md) §42, [views.md](views.md) §1). It
  may read the main database, the session's temp tables, and any attachment by its qualifier.
  PostgreSQL's `AS TABLE name` and `AS EXECUTE name` forms are absent, as jed has neither query form.
- **A data-modifying `WITH`.** The query's own `WITH` — written right after `AS`, or inside the
  query's parentheses (`AS (WITH … SELECT …)`) — may hold a data-modifying CTE
  (`CREATE TABLE t AS WITH d AS (DELETE FROM src … RETURNING *) SELECT * FROM d`). It is then the
  statement's writable `WITH` ([writable-cte.md](writable-cte.md)), as PostgreSQL treats the CTAS
  query as a top-level statement: its CTEs and its body are planned as a top-level `WITH`'s, and the
  fill (§3) runs them. A data-modifying CTE in a `WITH` nested any deeper — a derived table, a
  subquery, a set-operation arm — stays `0A000`, as in PostgreSQL ([cte.md](cte.md) §7).
- **The form is recognized after the table name** by the keyword `AS`, the keyword `ON` (an `ON
  COMMIT` clause comes before `AS`, as in PostgreSQL; the column-definition form needs `(` there), or
  a parenthesized list of bare names followed by `AS` or `ON`. A column definition always has a type
  after its name, so `( name [, name …] )` is never the column-definition form. No word becomes
  reserved.
- **`WITH DATA`** (the default) fills the table; **`WITH NO DATA`** creates it empty and never runs
  the query. The trailing `WITH [NO] DATA` is recognized at the end of the statement and cut off
  before the query is parsed, as `CREATE VIEW` cuts off its `WITH … CHECK OPTION` — an unreserved
  `with` would otherwise read as a table alias.
- **The column list** renames the query's output columns left to right. A shorter list keeps the rest
  of the query's names; a longer one is **`42601`** *too many column names were specified*.
- **Bind parameters.** The query's `$N` placeholders bind like any query's ([api.md](api.md) §5):
  typed from their context (an untyped one is `42P18`), counted (`42601` on a mismatch), and bound
  before anything is created. The column-definition form of `CREATE TABLE` still takes none.
- **Targets** follow `CREATE TABLE` exactly: a bare name is a persistent table in main, `TEMP` a
  session-local temp table ([temp-tables.md](temp-tables.md)), `main.` / `temp.` / an attachment
  qualifier a table in that database ([attached-databases.md](attached-databases.md) §3/§6), with
  `CREATE TABLE`'s `42601` / `42P01` / `25006` for a contradictory, unknown, or read-only target,
  the `jed_` reservation (`42939`), and the temp and attachment narrowings (a collated column on a
  temp or attached table is `0A000`, temp-tables.md §8).
- **Transactional.** The statement is DDL plus its rows, in the §3 staging model: it rolls back with
  its transaction, is refused in a read-only transaction (`25006`), and a failure anywhere in it —
  while filling included — leaves nothing behind.

## 2. The columns

A column is created for each output column of the query, in order, with:

- **its name** — the column list's, else the query's output name ([grammar.md](grammar.md) §8): a
  column keeps its name, an alias its alias, a function call the function's name, and anything else
  is `?column?`. Two columns with one final name are **`42701`** (so `AS SELECT 1, 2` is `42701`, as
  in PostgreSQL).
- **its type** — the query output's resolved type. An untyped `NULL` (and a string literal, already
  `text` in a select list) is `text`, PostgreSQL's `unknown` → `text`. An anonymous `ROW(…)` has no
  storable type: **`42P16`** *column "r" has pseudo-type record* (`record[]` for an array of one).
  A type no declared column may have in jed is the same `0A000` as for `CREATE TABLE`: a `jsonpath`
  column, and an array whose element is an array or a range.
- **its typmod** — the `numeric(p,s)` precision and scale or the `varchar(n)` length the output
  keeps, PostgreSQL's `exprTypmod`:
  - a column reference keeps its column's (a base table's, or a derived table's, CTE's, view's,
    `USING` join's, or set-returning function's output column, each of which records its body's);
  - a cast to `numeric(p,s)` / `varchar(n)` has the cast's; a cast without a typmod (`::decimal`,
    `::varchar`, `::text`) has none;
  - `COLLATE` keeps its operand's;
  - `CASE` (with an `ELSE`; without one it can yield a typmod-less `NULL`), `COALESCE`, `GREATEST`,
    and `LEAST` keep the typmod every result input has, else none;
  - `NULLIF(a, b)` keeps `a`'s (an integer `a` widened to decimal has none);
  - a set operation's or a `VALUES` list's column keeps the typmod every operand or row has, else
    none (PostgreSQL's `select_common_typmod`);
  - a scalar subquery keeps its column's;
  - a composite field — `(c).f`, `(c).*`, a nested field — keeps the field's declared typmod;
  - everything else — an operator, a function, an aggregate or window function, a literal, a
    parameter — has none.

  The typmod is a planning fact carried with each query output column, so the same rule types a
  derived table, CTE, or view reference's columns for the outer query, and a view's own columns
  report it: `jed_columns` renders a view column's type with it, and `CREATE OR REPLACE VIEW`
  compares it ([views.md](views.md) §1.2/§11). An array keeps no element typmod (§5).
- **its collation** (a `text` or `text[]` column) — the collation the output derives
  ([collation.md](collation.md) §1): a column's, an explicit `COLLATE`, a collatable function's or
  operator's propagated one. An output that derives none (a literal, an untyped `NULL`) takes the
  database's default collation, as a declared column without `COLLATE` does. An output whose
  collation cannot be derived — two different implicit collations meeting in a `UNION ALL` / `VALUES`
  column or an expression — is **`42P22`** *no collation was derived for column "x" with collatable
  type text*, as in PostgreSQL (which would otherwise have to invent one).
- **nothing else**: no `NOT NULL`, `DEFAULT`, `CHECK`, `UNIQUE`, primary key, foreign key, index, or
  identity is copied. The table has no primary key and so uses the synthetic rowid
  ([storage.md](storage.md)).

### 2.1 Recursive CTEs check the typmod too

PostgreSQL fixes a recursive CTE's column types — type *and* typmod — from its non-recursive term,
and a recursive term that changes them is `42804` (recursive-cte.md §2). jed already checked the
type; with typmods tracked it checks the typmod as well: `WITH RECURSIVE r(n) AS (SELECT price FROM
src UNION ALL SELECT n + 1 FROM r …)` over a `decimal(10,2)` column is **`42804`** *recursive query "r"
column 1 has type decimal(10,2) in non-recursive term but type decimal overall*, in any statement.

## 3. Execution

In PostgreSQL's order (parse analysis, then `CreateTableAsRelExists`, then `intorel_startup` /
`DefineRelation`):

1. The session envelope (`allow_ddl`, or `allow_temp_ddl` for a temp target, then the query's
   privileges — §4), `42501`.
2. The target database (`42601` / `42P01` / `25006`) and the `jed_` reservation (`42939`).
3. The query is planned, and its parameters typed and bound. Every error the query's analysis
   raises comes here — so `CREATE TABLE IF NOT EXISTS t AS SELECT * FROM nosuch` is `42P01` even when
   `t` exists, as in PostgreSQL.
4. A taken name: **`42P07`**, or under `IF NOT EXISTS` a silent no-op (PostgreSQL's notice; jed has
   no notice channel) that creates nothing and runs nothing. The collision is checked in the target
   database's namespace, temp-aware as for `CREATE TABLE` (temp-tables.md §3).
5. Per column, an underivable collation (`42P22`); then too many column names (`42601`); then `ON
   COMMIT` on a table that is not temporary (`42P16`); then a repeated column name (`42701`); then
   per column an unstorable type (`42P16` record, `0A000`).
6. The table is created through `CREATE TABLE`'s own registration, so every target rule and
   narrowing of §1 applies.
7. **`WITH DATA`**: the table is filled by running `INSERT INTO <db>.<name> <query>` in the same
   statement — the rows, their order (and so the synthetic rowids), the errors (`22012`, `22003`, …),
   and the **cost** are that `INSERT … SELECT`'s ([grammar.md](grammar.md) §24, [cost.md](cost.md)):
   the query's accrued cost, storing being unmetered. A failure rolls the statement back with the
   table. The affected-row count is the number of rows inserted. **`WITH NO DATA`** stops after step
   6: cost `0`, no row count, and the query is never run, so its runtime errors cannot fire.

   A query whose own `WITH` writes (§1) is filled by `WITH <its CTEs> INSERT INTO <db>.<name>
   <its body>` instead, run by the writable-CTE orchestrator ([writable-cte.md](writable-cte.md)
   §2–§6): every part reads the pre-statement snapshot — which holds the new, still-empty table — so
   the body sees a data-modifying CTE's `RETURNING` rows but not its writes; each data-modifying CTE
   runs exactly once, in list order, referenced or not; and the cost is that statement's. `WITH NO
   DATA` and an `IF NOT EXISTS` skip run no part of the query, so nothing is written.

The created columns' types equal the query's by construction, so the fill relies on `INSERT …
SELECT` accepting a source column of exactly a container column's type: an array, range, or
composite column takes a source column of the same type or an untyped `NULL`, the container rule
`UPDATE` and an `INSERT … VALUES` expression already share ([grammar.md](grammar.md) §24). That rule
now governs every `INSERT … SELECT` (formerly `0A000` for any container column).

`ON COMMIT` applies as for any temp table (temp-tables.md §5.1): an autocommit `CREATE TEMP TABLE t
ON COMMIT DROP AS …` creates, fills, and drops `t` in one statement; `ON COMMIT DELETE ROWS` empties
it at that commit.

## 4. Privileges

As PostgreSQL checks them ([session.md](session.md) §5.3): the DDL gate of `CREATE TABLE` (`allow_ddl`,
or `allow_temp_ddl` for a temp table), plus — because the query runs — `SELECT` on every relation and
`EXECUTE` on every function the query reads or calls, exactly as the same query would need. `WITH NO
DATA` never runs the query, so it needs nothing on what the query names (PostgreSQL plans no
executor for it). The new table needs no `INSERT` privilege to be filled (PostgreSQL 18 checks none).
A data-modifying CTE (§1) needs its write privilege on its target (session.md §5.3, as in any writable
`WITH`), again only under `WITH DATA`.

## 5. Divergences and deferred work

Recorded divergences from PostgreSQL (CLAUDE.md §1):

| Divergence | Reason |
|---|---|
| A bare integer literal's column (`AS SELECT 1 AS a`) is `i64`; PostgreSQL's is `integer` | a projected integer literal is `i64` in jed ([types.md](types.md) §6) |
| An array column keeps no element typmod: `ARRAY[name]` over a `varchar(5)`, `ARRAY(SELECT price …)`, and `x::varchar(3)[]` make `text[]` / `decimal[]`; PostgreSQL keeps `varchar(5)[]` | a typmod on an array element is a deferred column form (`0A000`, [array.md](array.md) §12) |
| A filled `INTERSECT` / `EXCEPT` column whose operands' typmods differ has none; PostgreSQL's planner reports the left operand's (`numeric(10,2)` for `SELECT price … INTERSECT SELECT 1.5`) | jed uses the analysis-level common typmod, PostgreSQL's own answer under `WITH NO DATA` and for `UNION` |
| A missing bound parameter is `42601` (*bind parameter count mismatch*); PostgreSQL's simple protocol says `42P02` | jed's parameter binding, the same for every query (api.md §5) |
| A query that reads a relation the session may not read is `42501` before the name check, so even a skipped `IF NOT EXISTS` raises it; PostgreSQL checks privileges when the query runs, after the skip | jed authorizes a statement before running any of it ([session.md](session.md) §5.3) |
| `CREATE TEMP TABLE IF NOT EXISTS p AS …` with a persistent `p` creates nothing; PostgreSQL creates a shadowing `pg_temp.p` | the preclude-overlaps temp namespace (temp-tables.md §3) |
| A collated column on a temp or attached table is `0A000` | the temp / attachment narrowing of `CREATE TABLE` (temp-tables.md §8, attached-databases.md §8) |
| `CREATE UNLOGGED TABLE` and `SELECT … INTO UNLOGGED` are `0A000`; PostgreSQL creates an unlogged table | every jed table is durable (CLAUDE.md §9); jed has no unlogged storage (§6) |
| `EXPLAIN ANALYZE CREATE TABLE … AS` in a read-only transaction is `25006`; PostgreSQL 18 runs it and creates the table | EXPLAIN ANALYZE of a write is a write ([explain.md](explain.md) §1); PostgreSQL's plain `CREATE TABLE … AS` is refused there too |
| `EXPLAIN ANALYZE CREATE TABLE … AS … WITH NO DATA` runs no part of the query; PostgreSQL runs its data-modifying CTEs (its executor starts the plan without fetching a row, then finishes the CTEs) | `WITH NO DATA` never runs the query (§3), and ANALYZE runs the statement as written (§7) |

Deferred follow-ons, none foreclosed:

- array element typmods, when array columns can declare them;
- `LOCAL` / `GLOBAL` before `TEMP` (PostgreSQL's deprecated spellings), absent from `CREATE TABLE`
  too.

## 6. `SELECT … INTO`

```
SELECT [ DISTINCT ] select_items INTO [ TEMPORARY | TEMP ] [ TABLE ] [ db . ] name [ FROM … ] …
```

PostgreSQL's older spelling of `CREATE [TEMP] TABLE name AS <the query without the INTO clause>`: the
same statement, with the same columns (§2), checks and their order (§3), rows, cost, privileges (§4),
and result (the inserted row count). The clause has no column list, `ON COMMIT`, `IF NOT EXISTS`, or
`WITH [NO] DATA` — a taken name is `42P07`. It follows the select list (before `FROM`), as in
PostgreSQL; anywhere after it is a syntax error.

**Where it may appear.** Only on the statement's **leftmost `SELECT`**: the first `SELECT` of a query
statement — its first set-operation arm, inside its parentheses, or the primary query of a `WITH`
(whose CTEs, a data-modifying one included, become the CTAS query's own `WITH`, §1) — or of the
query `EXPLAIN` takes. Everywhere else it is refused, with PostgreSQL's codes and messages:

| Position | Error |
|---|---|
| a later set-operation arm (`SELECT … UNION SELECT … INTO t`), parenthesized or not | `42601` *INTO is only allowed on first SELECT of UNION/INTERSECT/EXCEPT* |
| a subquery, derived table, CTE body, `INSERT` source, the query of `CREATE TABLE … AS`, or a `WITH` whose primary is DML | `42601` *SELECT ... INTO is not allowed here* |
| the leftmost `SELECT` of a `CREATE VIEW` body | `0A000` *views must not contain SELECT INTO* |
| after a `VALUES` list | `42601` (a syntax error, as in PostgreSQL) |

The parser decides this: the first `select_core` of a query statement owns the clause, and every
`SELECT` nested in its clauses, in a CTE body, or after a `VALUES` operand does not.

**The target words.** `TEMP` / `TEMPORARY` make the table temporary, and `TABLE` is optional; each is
a word of the clause unless a `.` follows it, so `INTO temp.t` names the `temp` database. A qualified
target follows `CREATE TABLE`'s rules (`main.`, `temp.`, an attachment; `INTO TEMP main.t` is
`42601`). `UNLOGGED` is recognized and refused `0A000` *unlogged tables are not supported*, as is
`CREATE UNLOGGED TABLE` (§5): every jed table is durable.

## 7. `EXPLAIN [ANALYZE] CREATE TABLE … AS`

`EXPLAIN` accepts a `CREATE [TEMP] TABLE … AS` (and so a `SELECT … INTO`) as its statement; the
column-definition form of `CREATE TABLE` stays `42601` ([explain.md](explain.md) §1). It renders the
plan of the **query the statement runs**, exactly as `EXPLAIN` of that query statement renders it —
the same nodes, estimates, and details, and for a data-modifying `WITH` (§1) the complete `WITH` tree
([explain.md](explain.md) §6). PostgreSQL renders the query's plan the same way, with no node of its
own for the table.

Before rendering, the checks `CREATE TABLE … AS` makes before anything exists run in its order (§3
steps 1–4): the privileges, the target database and the `jed_` reservation, the query's analysis and
its parameters, then the name — `42P07`, or under `IF NOT EXISTS` a taken name renders **no plan
rows** (PostgreSQL's empty plan and notice). Plain `EXPLAIN` stops there: it creates nothing, is a
read (so it runs in a read-only transaction), and the column checks of §3 step 5 (`42P22`, `42601`,
`42P16`, `42701`, the column types) do not run — PostgreSQL makes them only when the table is
defined.

`EXPLAIN ANALYZE` runs the statement: it creates and fills the table (a write, committed by an
autocommit invocation), and its `Analyze` root reports the statement's cost and inserted row count
([explain.md](explain.md) §3), with every rendered node's `actual_cost` attributed from that run — the
fill is the query's own execution, so the attribution is that of `EXPLAIN ANALYZE` of the query.
Under `WITH NO DATA` it creates the empty table and runs nothing of the query: cost `0`, rows `0`,
and every `actual_cost` `0`. The `LANE` tag is `buffered`, the lane of every write.
