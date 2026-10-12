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

	const replace = `CREATE OR REPLACE VIEW active_staff AS
  SELECT id, name, dept, salary FROM employee WHERE active AND dept = 'eng';
SELECT * FROM active_staff ORDER BY id;`;

	const renameView = `CREATE VIEW eng_staff AS SELECT name FROM active_staff WHERE dept = 'eng';
ALTER VIEW active_staff RENAME TO on_duty;
ALTER VIEW on_duty RENAME COLUMN dept TO team;
SELECT name, definition FROM jed_views ORDER BY name;`;

	const writeSeed = `CREATE TABLE task (
  id    i32 PRIMARY KEY,
  title text NOT NULL,
  done  boolean NOT NULL DEFAULT false,
  owner text
);
INSERT INTO task VALUES
  (1, 'write docs', false, 'ada'),
  (2, 'ship it',    true,  'ada'),
  (3, 'review',     false, 'ken');
CREATE VIEW open_tasks AS
  SELECT id, title, upper(owner) AS owner_tag FROM task WHERE NOT done;`;

	const writeQuery = `UPDATE open_tasks SET title = 'write more docs' WHERE id = 1;
INSERT INTO open_tasks (id, title) VALUES (4, 'plan');
DELETE FROM open_tasks WHERE owner_tag = 'KEN';
SELECT * FROM task ORDER BY id;`;
</script>

<svelte:head>
	<title>Views — jed</title>
	<meta name="description" content="CREATE [OR REPLACE] VIEW, ALTER VIEW, and DROP VIEW in jed — named, persisted queries that read exactly like the equivalent subquery, writable when they select from one table, with dependency tracking, rename rewriting, and per-view privileges." />
</svelte:head>

# Views

A **view** is a named, saved query. Once created, it can be used in `FROM` like a table:

```sql
CREATE VIEW active_staff AS
  SELECT id, name, dept FROM employee WHERE active;

SELECT name FROM active_staff WHERE dept = 'eng';
```

A view stores no rows. Each time a query reads it, jed runs its saved query in place. A simple view
over one table can also be written to: `INSERT`, `UPDATE`, and `DELETE` on it change the table
underneath ([below](#writing-through-a-view)).

<LiveSql demo="query" {seed} {query} rows={4} />

Things to try in the panel above:

- Read a view — `SELECT dept, headcount, payroll FROM dept_payroll ORDER BY dept;`
- Rename a view's columns where you use it — `SELECT p, q FROM active_staff AS s (p, q) ORDER BY p;`
- See the stored definitions — `SELECT name, definition, columns FROM jed_views ORDER BY name;`
- Write through a view — `UPDATE active_staff SET dept = 'ops' WHERE id = 1;`
- Write to a view that groups its rows — `DELETE FROM dept_payroll;` &rarr; error `55000`
- Drop a table a view reads — `DROP TABLE employee;` &rarr; error `2BP01`
- Drop a column a view reads — `ALTER TABLE employee DROP COLUMN salary;` &rarr; error `2BP01`
- Drop both views in one statement — `DROP VIEW active_staff, dept_payroll;`
- Replace a view with fewer columns — `CREATE OR REPLACE VIEW active_staff AS SELECT id, name FROM employee;` &rarr; error `42P16`
- Rename a view — `ALTER VIEW dept_payroll RENAME TO payroll;`

## Creating a view

```sql
CREATE [OR REPLACE] VIEW [IF NOT EXISTS] name [(column, …)] AS query
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

`CREATE VIEW`, `ALTER VIEW`, and `DROP VIEW` are ordinary DDL. They are transactional and roll back
with their transaction.

## Reading a view

Reading a view behaves exactly like writing its query as a subquery in `FROM`. `FROM active_staff`
is the same as `FROM (SELECT id, name, dept FROM employee WHERE active) AS active_staff`. Both
return the same rows and errors, cost the same, and get the same plan. jed adds no cost of its own
for the view, and the planner can push a filter into the view's query, so an outer
`WHERE id = 2` still becomes a primary-key lookup on `employee`:

<LiveSql demo="explain" {seed} query={explain} rows={3} />

A view's query cannot see anything from the query that reads it: not its other tables, not its
`WITH` names, and not an enclosing query's columns. A view can read other views.

Each view reference expands to its whole query, including the views that query reads. One reference
may expand to at most **1024** views, counting itself and every view reference reached through it.
More is error `54001`, at `CREATE VIEW` too. This keeps views that each read the previous view
twice, which double at every level, from making planning unbounded. PostgreSQL has no such limit.

The expanded query must also stay within the **256**-level nesting limit that applies to every
statement. A view's query counts from the depth at which the view is read, as if the reference were
replaced by the query in parentheses, so a chain of views with deeply nested queries is `54001` once
the whole chain would be too deep, at `CREATE VIEW` or when it is read.

As with any query, rows come back in a defined order only when the outer query has an `ORDER BY`.

## `*` is fixed when the view is created

As in PostgreSQL, `SELECT *` in a view means the columns that exist **when the view is created**. A
column added to the table later does not appear in the view:

<LiveSql demo="starQuery" seed={starSeed} query={starQuery} rows={3} />

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

<LiveSql demo="rename" {seed} query={rename} rows={2} />

After the renames, `SELECT name FROM active_staff` still works and returns the same rows.

## Replacing a view

`CREATE OR REPLACE VIEW` changes the query of an existing view, or creates the view if it does not
exist yet. The views that read it keep working and see the new query. As in PostgreSQL, the new
query must keep the view's existing columns, with the same names, types, and collations in the same
order. It may add new columns at the end:

<LiveSql demo="replace" {seed} query={replace} rows={2} />

A change to an existing column is `42P16`: fewer columns, a different name, a different type, or a
different collation. To rename a column, use `ALTER VIEW … RENAME COLUMN` (below). Two details
differ from PostgreSQL:

- Types are compared without their length or precision, so `varchar(3)` can become `varchar(4)` or
  `text`. A bare integer such as `0` is an `i64` in jed, so `0 AS n` cannot replace an `i32` column;
  write `0::i32 AS n`.
- A replacement whose query would read the view itself, directly or through other views, is `42P17`.
  PostgreSQL accepts it and fails every later read.

Naming a table, index, or sequence is `42809`. `OR REPLACE` cannot be combined with `IF NOT EXISTS`
(`42601`).

## Renaming a view

```sql
ALTER VIEW [IF EXISTS] name RENAME TO new_name
ALTER VIEW [IF EXISTS] name RENAME [COLUMN] column TO new_column
```

`ALTER TABLE` accepts the same two forms for a view. The views that read the renamed view keep
working with the same column names: jed rewrites their stored definitions, as it does for a renamed
table.

<LiveSql demo="renameView" {seed} query={renameView} rows={3} />

A missing view is `42P01` unless you write `IF EXISTS`. Naming a table, index, or sequence is
`42809`. A new name that is taken is `42P07`. Renaming a column that does not exist is `42703`, and
to a name the view already has is `42701`. The other `ALTER VIEW` forms, such as `OWNER TO` and
`SET (…)`, are not supported (`0A000`).

## Other table changes are checked

Because a view is stored as text and its names are resolved each time it is read, a table change
could make a name in a view refer to a different column. After every `ALTER TABLE`, jed checks each
view that reads the table. If the view's names would now resolve differently, the `ALTER TABLE`
fails with `0A000` and nothing changes. For example, adding a column named `x` fails when a view's
query uses a bare `x` that meant a column of another table.

The same check runs after `CREATE OR REPLACE VIEW` adds a column: it fails with `0A000` when a view
that reads the replaced view uses a bare name that the new column would capture.

PostgreSQL accepts such changes. To avoid the error, qualify column references in view queries
(`e.name` rather than `name`).

<a id="writing-through-a-view"></a>

## Writing through a view

As in PostgreSQL, a view whose query reads exactly one table (or one such view), with no `DISTINCT`,
`GROUP BY`, `HAVING`, `LIMIT`, `OFFSET`, `WITH`, aggregate, window function, or set operation, is
**updatable**. `INSERT`, `UPDATE`, and `DELETE` on it, and a data-modifying `WITH` that targets it,
write the table underneath:

<LiveSql demo="writeQuery" seed={writeSeed} query={writeQuery} rows={4} />

- A column that is a plain column of the table (`id`, `title` above) can be written. Any other
  column (`owner_tag`, computed with `upper`) can be read in `WHERE`, `SET`, and `RETURNING`, but
  assigning it is `0A000`.
- `UPDATE` and `DELETE` reach only the rows the view shows: its `WHERE` is applied first, and only
  the rows it keeps are tested against the statement's own `WHERE`. Task 2 is done, so the view
  cannot change it.
- `INSERT` fills every table column the view does not set with its default (`done` becomes
  `false`). The view's `WHERE` is not checked on insert, so an inserted row may be one the view
  does not show (`WITH CHECK OPTION` is not supported yet).
- `UPDATE v SET c = DEFAULT` sets `c` to `NULL`, as in PostgreSQL, because a view column has no
  default of its own.
- `RETURNING` returns the view's columns, including computed ones.
- A view over a view writes through both, down to the table.
- The table's own rules still apply: `NOT NULL`, `CHECK`, `UNIQUE`, primary and foreign keys.

A write through a view is planned like the same write on the table: `UPDATE open_tasks … WHERE id =
1` is a primary-key lookup on `task`, and `EXPLAIN` shows `Update task`.

Writing to any other view, such as `dept_payroll` (which groups its rows), is `55000` (*cannot
delete from view …*). `INSERT … ON CONFLICT` on a view is not supported yet (`0A000`).

`CREATE INDEX … ON` a view and a foreign key that references a view are `42809`. `ANALYZE` on a view
succeeds and does nothing.

## Privileges

A view is a privilege boundary, as in PostgreSQL:

- **Reading a view** needs `SELECT` on the **view** only. The session reading it needs no privilege
  on the tables inside. A host can use this to expose some columns or rows of a table to a session
  that cannot read the table itself.
- **Creating or replacing a view** needs `allow_ddl` and `SELECT` on every table and view its query
  reads, checked against the creating session (`42501`). A session cannot create a view over data it
  could not read itself. Renaming a view needs `allow_ddl`.
- **Writing through a view** is checked like writing a table: `INSERT`, `UPDATE`, or `DELETE` on the
  view, plus `SELECT` on it when the statement reads the view's columns. The table underneath is not
  checked, so a host can let a session change some rows of a table without any grant on the table.
  (PostgreSQL also checks the view owner's privileges on the table; jed has no owners.)
- Functions the view calls still need `EXECUTE` for the session reading or writing the view.

Privileges are configured by the host. See [Authorization](../../api/authorization/).

## Introspection

`jed_views` lists each view's `name`, its stored `definition` (normalized: lowercase keywords,
single-spaced tokens, `*` expanded, renames applied), and its output `columns` as a `text[]`. Views
are not listed in `jed_tables` or `jed_columns`. To learn a view's column types, prepare
`SELECT * FROM v`. See [Introspection](../introspection/).

## Not supported yet

Each of these is `0A000` today:

- `ALTER VIEW` forms other than `RENAME`.
- `WITH CHECK OPTION` and `WITH (options)`.
- `INSERT … ON CONFLICT` on a view.
- `CREATE MATERIALIZED VIEW` and `CREATE RECURSIVE VIEW`.
- Temporary views, and a view that reads a temporary table.
- Views in an attached database, and views whose query reads an attached database.
- `DROP … CASCADE` that would drop a dependent view.
