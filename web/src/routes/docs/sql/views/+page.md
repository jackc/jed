<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TABLE employee (
  id     i32 PRIMARY KEY,
  name   text NOT NULL,
  dept   text NOT NULL,
  salary numeric(9,2) NOT NULL,
  active boolean NOT NULL
);
INSERT INTO employee VALUES
  (1, 'Ada',   'eng',   9500.00, true),
  (2, 'Grace', 'eng',   9900.00, true),
  (3, 'Linus', 'ops',   7200.00, true),
  (4, 'Ken',   'ops',   6800.00, false),
  (5, 'Barb',  'sales', 5100.00, true);
CREATE VIEW active_staff AS
  SELECT id, name, dept FROM employee WHERE active;
CREATE VIEW dept_payroll (dept, headcount, payroll) AS
  SELECT dept, count(*), sum(salary)
  FROM employee
  WHERE active
  GROUP BY dept;`;

	const query = `SELECT s.name, s.dept, p.payroll
FROM active_staff AS s
JOIN dept_payroll AS p USING (dept)
ORDER BY s.id;`;

	const explain = `EXPLAIN SELECT name FROM active_staff WHERE id = 2;`;

	const starSeed = `CREATE TABLE note (id i32 PRIMARY KEY, body text);
INSERT INTO note VALUES (1, 'hello'), (2, 'world');
CREATE VIEW all_notes AS SELECT * FROM note;
ALTER TABLE note ADD COLUMN pinned boolean DEFAULT false;`;

	const starQuery = `SELECT * FROM all_notes ORDER BY id;`;

	const rename = `ALTER TABLE employee RENAME COLUMN name TO full_name;
ALTER TABLE employee RENAME TO staff;
SELECT name, definition FROM jed_views ORDER BY name;`;
</script>

<svelte:head>
	<title>Views — jed</title>
	<meta name="description" content="CREATE VIEW and DROP VIEW in jed — named, persisted, read-only queries that read exactly like the equivalent subquery, with dependency tracking, rename rewriting, and per-view privileges." />
</svelte:head>

# Views

A **view** is a named, saved query. Once created, it can be used in `FROM` like a table:

```sql
CREATE VIEW active_staff AS
  SELECT id, name, dept FROM employee WHERE active;

SELECT name FROM active_staff WHERE dept = 'eng';
```

A view stores no rows. Each time a query reads it, jed runs its saved query in place. Views are
**read-only** in this release.

<LiveSql {seed} {query} rows={4} />

Things to try in the panel above:

- Read a view — `SELECT dept, headcount, payroll FROM dept_payroll ORDER BY dept;`
- Rename a view's columns where you use it — `SELECT p, q FROM active_staff AS s (p, q) ORDER BY p;`
- See the stored definitions — `SELECT name, definition, columns FROM jed_views ORDER BY name;`
- Write to a view — `INSERT INTO active_staff VALUES (6, 'Dennis', 'eng');` &rarr; error `55000`
- Drop a table a view reads — `DROP TABLE employee;` &rarr; error `2BP01`
- Drop a column a view reads — `ALTER TABLE employee DROP COLUMN salary;` &rarr; error `2BP01`
- Drop both views in one statement — `DROP VIEW active_staff, dept_payroll;`

## Creating a view

```sql
CREATE VIEW [IF NOT EXISTS] name [(column, …)] AS query
```

The query can be anything a subquery in `FROM` accepts: joins, aggregates, window functions,
`WITH` (including `WITH RECURSIVE`), set operations, `ORDER BY`, and `LIMIT`. It cannot use
parameters (`$1` is `42P02`) or a data-modifying `WITH` (`0A000`).

The view's column names come from the query, following the usual output-name rules. An optional
column list renames them from left to right, as `dept_payroll` does above. A shorter list renames only
the leading columns; a longer one is `42601`. Two columns with the same final name are `42701`, so
give computed columns a name: `CREATE VIEW v AS SELECT 1, 2` fails because both columns are named
`?column?`.

A view shares its name space with tables, indexes, and sequences, so a taken name is `42P07`.
`CREATE VIEW IF NOT EXISTS` makes that a silent no-op instead. This is a jed extension, as in SQLite;
PostgreSQL has no such form.

`CREATE VIEW` and `DROP VIEW` are ordinary DDL. They are transactional and roll back with their
transaction.

## Reading a view

Reading a view behaves exactly like writing its query as a subquery in `FROM`. `FROM active_staff`
is the same as `FROM (SELECT id, name, dept FROM employee WHERE active) AS active_staff`. Both
return the same rows and errors, cost the same, and get the same plan. jed adds no cost of its own
for the view, and the planner can push a filter into the view's query, so an outer
`WHERE id = 2` still becomes a primary-key lookup on `employee`:

<LiveSql {seed} query={explain} rows={3} />

A view's query cannot see anything from the query that reads it: not its other tables, not its
`WITH` names, and not an enclosing query's columns. A view can read other views.

Each view reference expands to its whole query, including the views that query reads. One reference
may expand to at most **1024** views, counting itself and every view reference reached through it.
More is error `54001`, at `CREATE VIEW` too. This keeps views that each read the previous view
twice, which double at every level, from making planning unbounded. PostgreSQL has no such limit.

As with any query, rows come back in a defined order only when the outer query has an `ORDER BY`.

## `*` is fixed when the view is created

As in PostgreSQL, `SELECT *` in a view means the columns that exist **when the view is created**. A
column added to the table later does not appear in the view:

<LiveSql seed={starSeed} query={starQuery} rows={3} />

jed stores the expanded column list in the view's definition, so `jed_views.definition` shows the
explicit columns rather than `*`.

## Dropping a view

```sql
DROP VIEW [IF EXISTS] name [, …] [RESTRICT | CASCADE]
```

A missing view is `42P01` unless you write `IF EXISTS`. Naming a table (or index or sequence) is
`42809` (*not a view*), even with `IF EXISTS`. A list is all-or-nothing, and a view that depends on
another view in the same list does not block the drop. `DROP VIEW` on a view that another view reads
is `2BP01`. `CASCADE`, which would also drop the dependent views, is not supported yet (`0A000`).

## Dependencies

jed tracks which tables and columns each view reads and refuses changes that would break it:

| Statement | Result |
|---|---|
| `DROP TABLE t` when a view reads `t` | `2BP01` |
| `DROP VIEW v` when another view reads `v` | `2BP01` |
| `ALTER TABLE t DROP COLUMN c` when a view reads `c` | `2BP01` |
| `ALTER TABLE t ALTER COLUMN c TYPE …` when a view reads `c` | `0A000` |
| `CASCADE` on any of these, when the dependent is a view | `0A000` (not supported yet) |

The error message names the blocking view. Drop the dependent views first, then the table or column.

Dependencies cover tables, columns, and other views. A view does **not** track the sequences,
composite types, or collations its query names. Dropping one of those succeeds, and the view then
fails with the ordinary error when it is next read. (PostgreSQL refuses the drop for sequences and
types.)

## Renaming tables and columns

Renaming a table or column that a view reads keeps the view working, with the same column names.
jed rewrites each affected view's stored definition, using a [column alias list](../select/)
(`FROM staff AS employee (id, name)`) to keep the old names. Run this to see the rewritten text:

<LiveSql {seed} query={rename} rows={2} />

After the renames, `SELECT name FROM active_staff` still works and returns the same rows.

## Other table changes are checked

Because a view is stored as text and its names are resolved each time it is read, a table change
could make a name in a view refer to a different column. After every `ALTER TABLE`, jed checks each
view that reads the table. If the view's names would now resolve differently, the `ALTER TABLE`
fails with `0A000` and nothing changes. For example, adding a column named `x` fails when a view's
query uses a bare `x` that meant a column of another table.

PostgreSQL accepts such changes. To avoid the error, qualify column references in view queries
(`e.name` rather than `name`).

## Views are read-only

`INSERT`, `UPDATE`, `DELETE`, `INSERT … ON CONFLICT`, and a data-modifying `WITH` that targets a
view all fail with `55000` (*cannot insert into view …*). PostgreSQL can write through simple
single-table views; jed does not support that yet. Write to the underlying table instead.

`CREATE INDEX … ON` a view and a foreign key that references a view are `42809`. `ANALYZE` on a view
succeeds and does nothing.

## Privileges

A view is a privilege boundary, as in PostgreSQL:

- **Reading a view** needs `SELECT` on the **view** only. The session reading it needs no privilege
  on the tables inside. A host can use this to expose some columns or rows of a table to a session
  that cannot read the table itself.
- **Creating a view** needs `allow_ddl` and `SELECT` on every table and view its query reads,
  checked against the creating session (`42501`). A session cannot create a view over data it could
  not read itself.
- Functions the view calls still need `EXECUTE` for the session reading the view.

Privileges are configured by the host. See [Authorization](../../api/authorization/).

## Introspection

`jed_views` lists each view's `name`, its stored `definition` (normalized: lowercase keywords,
single-spaced tokens, `*` expanded, renames applied), and its output `columns` as a `text[]`. Views
are not listed in `jed_tables` or `jed_columns`. To learn a view's column types, prepare
`SELECT * FROM v`. See [Introspection](../introspection/).

## Not supported yet

Each of these is `0A000` today:

- `CREATE OR REPLACE VIEW`, `ALTER VIEW`, and renaming a view (`ALTER TABLE v RENAME …`).
- `WITH CHECK OPTION` and `WITH (options)`.
- Writing through a view (updatable views).
- `CREATE MATERIALIZED VIEW` and `CREATE RECURSIVE VIEW`.
- Temporary views, and a view that reads a temporary table.
- Views in an attached database, and views whose query reads an attached database.
- `DROP … CASCADE` that would drop a dependent view.
