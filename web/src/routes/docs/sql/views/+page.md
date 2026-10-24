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

	const upsertQuery = `INSERT INTO open_tasks (id, title)
  VALUES (2, 'ship it again'), (5, 'triage')
  ON CONFLICT (id) DO UPDATE SET title = excluded.title
  RETURNING id, title, owner_tag;`;

	const checkSeed = `CREATE TABLE task (
  id    i32 PRIMARY KEY,
  title text NOT NULL,
  owner text
);
INSERT INTO task VALUES (1, 'write docs', 'ada'), (2, 'review', 'ken');
CREATE VIEW ada_tasks AS
  SELECT id, title, owner FROM task WHERE owner = 'ada'
  WITH CHECK OPTION;`;

	const checkQuery = `INSERT INTO ada_tasks VALUES (3, 'blog post', 'ada');
SELECT * FROM ada_tasks ORDER BY id;`;
</script>

<svelte:head>
	<title>Views — jed</title>
	<meta name="description" content="CREATE [OR REPLACE] VIEW, ALTER VIEW, and DROP VIEW in jed — named, persisted queries that read exactly like the equivalent subquery, writable when they select from one table, WITH CHECK OPTION, dependency tracking, rename rewriting, and per-view privileges." />
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
  [WITH [CASCADED | LOCAL] CHECK OPTION]
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
is `2BP01`. `CASCADE` drops the dependent views too, and every view that depends on those.

## Dependencies

jed tracks which tables and columns each view reads and refuses changes that would break it:

| Statement | Result |
|---|---|
| `DROP TABLE t` when a view reads `t` | `2BP01` |
| `DROP VIEW v` when another view reads `v` | `2BP01` |
| `ALTER TABLE t DROP COLUMN c` when a view reads `c` | `2BP01` |
| `ALTER TABLE t ALTER COLUMN c TYPE …` when a view reads `c` | `0A000` |
| `DROP TABLE t CASCADE`, `DROP VIEW v CASCADE`, `ALTER TABLE t DROP COLUMN c CASCADE` | drops the dependent views too |

The error message names the blocking view. Either drop the dependent views first, or add `CASCADE`.
`CASCADE` drops every view that depends on what the statement drops, then every view that depends
on one of those, and so on. `DROP COLUMN … CASCADE` drops only the views that read that column; the
table's other views keep working. Unlike PostgreSQL, jed does not print a *drop cascades to* notice,
so check `jed_views` if you need to know what went.

Dependencies also cover the **sequences** a view names in `nextval('s')`, `currval('s')`, or
`setval('s', …)` (a constant name; one that names no sequence is `42P01` at `CREATE VIEW`), and the
**composite types** it casts to or returns. `DROP SEQUENCE` and `DROP TYPE` of one is `2BP01`, and
`CASCADE` drops the dependent views. `ALTER SEQUENCE s RENAME TO s2` updates the views that use `s`.
Collations and host functions are not tracked: SQL cannot drop them, and a view whose collation or
function is missing fails with the ordinary error when it is read.

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
different collation. A type includes its length or precision, so `varchar(3)` cannot become
`varchar(4)` or `text`, and `numeric(5,2)` cannot become `numeric(6,2)`. To rename a column, use
`ALTER VIEW … RENAME COLUMN` (below). Two details differ from PostgreSQL:

- A bare integer such as `0` is an `i64` in jed, so `0 AS n` cannot replace an `i32` column; write
  `0::i32 AS n`. A `varchar` with no length is the same type as `text`, so one can replace the other.
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
to a name the view already has is `42701`. `OWNER TO` and `SET SCHEMA` are not supported (`0A000`):
jed has no owners or schemas.

## Column defaults

A view column can have a default of its own, used when writing through the view:

```sql
ALTER VIEW open_tasks ALTER COLUMN title SET DEFAULT 'untitled';
ALTER VIEW open_tasks ALTER COLUMN title DROP DEFAULT;
```

An `INSERT` through the view fills a column it leaves out, or writes as `DEFAULT`, from the view's
default before the table's; through a chain of views, the default of the view nearest the statement
wins. `UPDATE … SET c = DEFAULT` assigns the view's default, or `NULL` when it has none. The default is
checked like a table column's (`42804` for the wrong type, `0A000` for a column reference or
subquery). A default on a computed column makes every `INSERT` through the view fail with `0A000`, as
in PostgreSQL. `ALTER TABLE v ALTER COLUMN c SET DEFAULT …` works on a view too, and
`jed_columns.column_default` shows the default.

## View options

```sql
CREATE VIEW v WITH (security_invoker) AS SELECT …;
ALTER VIEW v SET (check_option = local, security_barrier);
ALTER VIEW v RESET (security_barrier);
```

The options are PostgreSQL's:

- `check_option` (`local` or `cascaded`) is another way to write `WITH … CHECK OPTION`.
- `security_invoker` checks the tables the view reads against the session reading it (and, for a
  write, the table underneath against the session writing), instead of making the view a privilege
  boundary ([below](#privileges)).
- `security_barrier` is accepted and stored. jed only moves outer conditions that cannot raise an
  error into a view, so running one on a row the view's own `WHERE` rejects reveals nothing, and the
  option changes nothing.

Values are booleans in PostgreSQL's spellings (`true`, `on`, `yes`, `1`, …); a bare name means
`true`. An unknown option or a bad value is `22023`. `CREATE OR REPLACE VIEW` replaces the options,
and `jed_views` shows them.

## Recursive views

```sql
CREATE RECURSIVE VIEW countdown (n) AS
  VALUES (3) UNION ALL SELECT n - 1 FROM countdown WHERE n > 1;
```

This is PostgreSQL's shorthand for a view over `WITH RECURSIVE countdown (n) AS (…) SELECT n FROM
countdown`, and jed stores it that way. The column list is required.

<a id="temporary-views"></a>

## Temporary views

`CREATE TEMP VIEW` (or `CREATE VIEW temp.v`) makes a view that only the current session sees and that
goes away when the session ends. A view that reads a temporary table is temporary too, as in
PostgreSQL (jed prints no notice). A temporary view can read temporary and persistent tables, and
dropping or altering a table it reads in the same session is checked as for any view. Another
session's statements do not see it, so they are not blocked by it. `temp.jed_views` lists the
session's temporary views. Temporary view DDL is allowed by `allow_temp_ddl`.

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
  does not show, and an `UPDATE` may move a row out of the view. `WITH CHECK OPTION`
  ([below](#with-check-option)) prevents both.
- `UPDATE v SET c = DEFAULT` sets `c` to `NULL`, as in PostgreSQL, because a view column has no
  default of its own.
- `RETURNING` returns the view's columns, including computed ones.
- A view over a view writes through both, down to the table.
- The table's own rules still apply: `NOT NULL`, `CHECK`, `UNIQUE`, primary and foreign keys.

A write through a view is planned like the same write on the table: `UPDATE open_tasks … WHERE id =
1` is a primary-key lookup on `task`, and `EXPLAIN` shows `Update task`.

Writing to any other view, such as `dept_payroll` (which groups its rows), is `55000` (*cannot
delete from view …*).

### `INSERT … ON CONFLICT` through a view

`ON CONFLICT` works through an updatable view as it does on the table underneath, as in PostgreSQL.
Everything in the clause is written in the view's columns:

<LiveSql demo="upsertQuery" seed={writeSeed} query={upsertQuery} rows={2} />

- The conflict target `(id)` names view columns, each mapped to its table column, and must match the
  table's primary key or a `UNIQUE` constraint. A computed column such as `owner_tag` matches none
  (`42P10`), and `ON CONSTRAINT` is `42704`, because a view has no constraints.
- `DO UPDATE SET` assigns view columns (a computed one is `0A000`, and `= DEFAULT` sets the view
  column's default, or `NULL`).
  `excluded.col` and the view's name (or the `AS` alias) read the proposed and the existing row
  through the view's columns, computed ones included.
- The view's `WHERE` does not limit which existing row is updated: task 2 is done, so the view does
  not show it, yet the conflict updates it. `WITH CHECK OPTION` ([below](#with-check-option)) checks
  every row the statement inserts or updates; a row `DO NOTHING` skips is not checked.

`CREATE INDEX … ON` a view and a foreign key that references a view are `42809`. `ANALYZE` on a view
succeeds and does nothing.

<a id="with-check-option"></a>

## `WITH CHECK OPTION`

A view created `WITH CHECK OPTION` refuses an `INSERT` or `UPDATE` through it whose new row the view
would not show:

<LiveSql demo="checkQuery" seed={checkSeed} query={checkQuery} rows={2} />

Things to try in the panel above:

- Insert a row the view would hide — `INSERT INTO ada_tasks VALUES (4, 'triage', 'ken');` &rarr; error `44000`
- Move a row out of the view — `UPDATE ada_tasks SET owner = 'ken' WHERE id = 1;` &rarr; error `44000`
- Delete through the view — `DELETE FROM ada_tasks WHERE id = 1;` (`DELETE` is never checked)

The new row must make the view's `WHERE` true. False **or NULL** fails, so an `INSERT` that leaves
`owner` out is refused too. The check sees every column of the new row, including table columns the
view does not show, which an `INSERT` fills with their defaults.

When a view reads another view, the check option says which `WHERE` clauses are checked:

- **`WITH CASCADED CHECK OPTION`** (the default for plain `WITH CHECK OPTION`) checks the view's own
  `WHERE` and the `WHERE` of every view below it.
- **`WITH LOCAL CHECK OPTION`** checks the view's own `WHERE`, plus the views below that have a check
  option of their own.

The views are checked from the one nearest the table up, and the error (`44000`, *new row violates
check option for view …*) names the first one that fails. The table's own `NOT NULL`, `CHECK`,
primary-key, and `UNIQUE` checks come first, as in PostgreSQL.

Only an updatable view can have a check option: `CREATE VIEW` of any other view `WITH CHECK OPTION`
is `0A000`. `CREATE OR REPLACE VIEW` replaces the check option along with the query, so leaving the
clause out removes it. `jed_views.check_option` shows `NONE`, `LOCAL`, or `CASCADED`.

The check evaluates the view's `WHERE` on each new row, so it adds that expression's cost to the
write. Two orderings differ from PostgreSQL: a row that violates both an `EXCLUDE` constraint and the
check option reports `44000`, and an `UPDATE` reports a later row's `CHECK` or `UNIQUE` failure
before an earlier row's check-option failure.

<a id="privileges"></a>

## Privileges

A view is a privilege boundary, as in PostgreSQL:

- **Reading a view** needs `SELECT` on the **view** only. The session reading it needs no privilege
  on the tables inside. A host can use this to expose some columns or rows of a table to a session
  that cannot read the table itself.
- **Creating or replacing a view** needs `allow_ddl` and `SELECT` on every table and view its query
  reads, checked against the creating session (`42501`). A session cannot create a view over data it
  could not read itself. Renaming a view needs `allow_ddl`.
- **A `security_invoker` view** also needs the reading session's `SELECT` on the tables and views its
  query reads, and a write through it needs the writer's privilege on the table underneath.
- **Writing through a view** is checked like writing a table: `INSERT`, `UPDATE`, or `DELETE` on the
  view, plus `SELECT` on it when the statement reads the view's columns. The table underneath is not
  checked, so a host can let a session change some rows of a table without any grant on the table.
  (PostgreSQL also checks the view owner's privileges on the table; jed has no owners.)
- Functions the view calls still need `EXECUTE` for the session reading or writing the view.

Privileges are configured by the host. See [Authorization](../../api/authorization/).

## Introspection

`jed_views` lists each view's `name`, its stored `definition` (normalized: lowercase keywords,
single-spaced tokens, `*` expanded, renames applied), its output `columns` as a `text[]`, and its
`check_option` (`NONE`, `LOCAL`, or `CASCADED`), and its `security_barrier` and `security_invoker`
options. Views
are not listed in `jed_tables`, but `jed_columns` lists each view column with the type a query
over the view produces (with no length or precision: `text`, not `varchar(80)`). See
[Introspection](../introspection/).

## Not supported yet

Each of these is `0A000` today:

- `CREATE MATERIALIZED VIEW`.
- `ALTER VIEW … OWNER TO` and `SET SCHEMA`.
- Views in an attached database, and views whose query reads an attached database.
