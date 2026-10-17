<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const orderedSeed = `CREATE TABLE city (
  id     i32 PRIMARY KEY,
  name   text NOT NULL,
  region i32 NOT NULL
);
CREATE INDEX city_region ON city (region);
INSERT INTO city VALUES
  (1, 'Tokyo',  1),
  (2, 'Osaka',  1),
  (3, 'Paris',  2),
  (4, 'Lyon',   2),
  (5, 'Kyoto',  1);`;

	const orderedQuery = `SELECT name FROM city WHERE region = 1 ORDER BY name;`;

	const sortSeed = `CREATE TABLE score (
  id     i32 PRIMARY KEY,
  player text NOT NULL,
  points i32
);
CREATE INDEX score_points ON score (points DESC);
INSERT INTO score VALUES
  (1, 'ada',   90),
  (2, 'grace', NULL),
  (3, 'linus', 75),
  (4, 'barb',  98),
  (5, 'ken',   61);`;

	const sortQuery = `SELECT player, points FROM score
ORDER BY points DESC
LIMIT 3;`;

	const exprSeed = `CREATE TABLE account (
  id    i32 PRIMARY KEY,
  email text NOT NULL
);
CREATE UNIQUE INDEX ON account (lower(email));
INSERT INTO account VALUES
  (1, 'Ada@Example.com'),
  (2, 'grace@example.com');`;

	const exprQuery = `SELECT id FROM account WHERE lower(email) = 'ada@example.com';`;

	const partialSeed = `CREATE TABLE orders (
  id       i32 PRIMARY KEY,
  status   text NOT NULL,
  customer i32  NOT NULL
);
CREATE INDEX orders_active ON orders (customer)
  WHERE status = 'active';
INSERT INTO orders VALUES
  (1, 'active',    10),
  (2, 'shipped',   10),
  (3, 'active',    20),
  (4, 'active',    10),
  (5, 'cancelled', 20);`;

	const partialQuery = `SELECT id FROM orders
WHERE status = 'active' AND customer = 10
ORDER BY id;`;

	const softSeed = `CREATE TABLE task (
  id         i32 PRIMARY KEY,
  title      text NOT NULL,
  priority   i32 NOT NULL,
  deleted_at timestamptz
);
CREATE INDEX task_live ON task (priority) WHERE deleted_at IS NULL;
INSERT INTO task VALUES
  (1, 'ship',   2, NULL),
  (2, 'triage', 1, '2024-01-01 00:00:00+00'),
  (3, 'review', 3, NULL),
  (4, 'plan',   1, NULL),
  (5, 'archive', 5, '2024-02-01 00:00:00+00');`;

	const softQuery = `SELECT title, priority FROM task
WHERE deleted_at IS NULL
ORDER BY priority
LIMIT 2;`;

	const nullSeed = `CREATE TABLE accounts (id int PRIMARY KEY, email text, deleted_at timestamptz);
CREATE UNIQUE INDEX active_email ON accounts (email) WHERE deleted_at IS NULL;
INSERT INTO accounts VALUES (1, 'ada@example.com', NULL), (2, 'ada@example.com', '2024-01-01 00:00:00+00');`;
	const nullQuery = `SELECT id FROM accounts WHERE deleted_at IS NULL AND email = 'ada@example.com';`;

	const ginSeed = `CREATE TABLE post (
  id    i32 PRIMARY KEY,
  title text NOT NULL,
  tags  i32[]
);
CREATE INDEX post_tags_gin ON post USING gin (tags);
INSERT INTO post VALUES
  (1, 'intro',   ARRAY[10, 20, 30]),
  (2, 'arrays',  ARRAY[20, 40]),
  (3, 'storage', ARRAY[40, 50]),
  (4, 'gin',     ARRAY[10, 20]),
  (5, 'empty',   '{}');`;

	const containsQuery = `SELECT title FROM post
WHERE tags @> ARRAY[10, 20]
ORDER BY id;`;

	const overlapsQuery = `SELECT title FROM post
WHERE tags && ARRAY[30, 40]
ORDER BY id;`;

	const memberQuery = `SELECT title FROM post
WHERE 20 = ANY(tags)
ORDER BY id;`;

	const equalQuery = `SELECT title FROM post
WHERE tags = ARRAY[10, 20]
ORDER BY id;`;

	const jsonbSeed = `CREATE TABLE event (
  id   i32 PRIMARY KEY,
  body jsonb
);
CREATE INDEX event_body_gin ON event USING gin (body);
INSERT INTO event VALUES
  (1, '{"kind": "signup", "plan": "pro", "tags": ["beta"]}'),
  (2, '{"kind": "signup", "plan": "free"}'),
  (3, '{"kind": "login", "device": {"os": "ios"}}'),
  (4, '{"kind": "login", "device": {"os": "android"}, "tags": ["beta"]}'),
  (5, '{"kind": "refund"}');`;

	const jsonbContainsQuery = `SELECT id FROM event
WHERE body @> '{"kind": "login", "device": {"os": "ios"}}'
ORDER BY id;`;

	const jsonbKeyQuery = `SELECT id FROM event
WHERE body ? 'tags'
ORDER BY id;`;

	const reindexQuery = `REINDEX TABLE city;
SELECT name FROM city WHERE region = 2 ORDER BY name;`;
</script>

<svelte:head>
	<title>Indexes — jed</title>
	<meta name="description" content="CREATE INDEX in jed — ordered B-tree indexes (with ASC/DESC and NULLS FIRST/LAST keys), expression and partial (WHERE) indexes, and GIN inverted indexes that accelerate array containment, overlap, membership, and exact equality, and jsonb containment and key existence, run live." />
</svelte:head>

# Indexes

An index speeds up a lookup **without changing the answer**. A query returns the same rows whether
or not an index exists — the index only changes *which rows are scanned* (and the deterministic
[cost](../select/) shown with each result). For a one-table `SELECT`, jed estimates the complete
scheduled pipeline for full, primary-key, ordered B-tree, GIN, GiST, and interval-set paths and
picks the cheapest; exact ties use a fixed access-kind order and then lowercased index name. Every
index stays up to date on each `INSERT`, `UPDATE`, and `DELETE` whether or not a particular query
selects it.

## Ordered indexes (the default)

`CREATE INDEX [name] ON table (column)` builds an ordered B-tree over the column. It accelerates
equality lookups — `WHERE column = …` — by seeking instead of scanning the whole table. The
`PRIMARY KEY` is itself an index, and a `UNIQUE` constraint is backed by a unique index. The
indexed column must be a key-encodable type: the integer widths, `boolean`, `uuid`, `timestamp`,
`timestamptz`, `date`, `interval`, the variable-width `text`/`bytea`/`numeric`, `f32`/`f64`
(through jed's byte-pinned total float order), and the three **container** keys — a **`range`**, an
**`array`** of a key-encodable scalar, and a **composite** (`CREATE TYPE`) whose fields are all
key-encodable — each sorting by the same total order as `<`/`ORDER BY`. An `interval` key sorts by
its canonical span, so `INTERVAL '1 mon'` and `INTERVAL '30 days'` index as one value; likewise a
discrete `range` is stored canonical, so `'[1,4]'::int4range` and `'[1,5)'::int4range` index as one.
A composite value keys by each field's own key in order (NULLs last per field). The one type that
still cannot be a key is an **array whose element is a composite**.

An index may combine several columns. A lookup can use any number of leading columns with `=`,
followed by a range (`<`, `<=`, `>`, `>=`, `BETWEEN`) on the next one, and every column type above
can take either role. So an index on `(region, name)` serves `region = 1 AND name > 'K'`, and an index
on a `text` column serves `name >= 'K' AND name < 'P'` on its own.

The `city` table below indexes its `region` code (`1` = Asia, `2` = Europe). Run the lookup, then
edit the `WHERE` to `region = 2` — the selective equality chooses the index and narrows the scan to
the matching rows. A broad range may deliberately choose the full scan instead, avoiding an index
walk followed by a table point lookup for nearly every row. Either way the result is the same:

<LiveSql demo="orderedQuery" seed={orderedSeed} query={orderedQuery} rows={6} />

The same ordered bound narrows an **`UPDATE` or `DELETE` target scan**. Equality, range, composite
prefix, expression-index, and eligible partial-index predicates gather every matching old row and
its storage key before jed begins the two-phase write, so changing the indexed column or even the
primary key cannot disturb the scan. An `IN` list on an indexed leading column uses a de-duplicated
set of index point probes. The complete `WHERE` is still rechecked for every candidate; only the
work changes, never which rows are updated or deleted.

An index name shares the namespace of tables, sequences, and views, so creating an index whose name
is taken is error `42P07`. `CREATE INDEX IF NOT EXISTS name ON …` makes it a no-op instead, for
every index kind, so setup scripts can be re-run safely. The name is required in this form. As in
PostgreSQL, the new definition is still checked first (an unknown column is still `42703`), but
nothing is built and the existing index is left as it was, even if the new definition differs.

```sql
CREATE INDEX IF NOT EXISTS city_region ON city (region);   -- no-op: city_region exists
```

The same rule applies inside a join: `parent JOIN child ON child.parent_id = parent.id` opens the
child index once per parent row instead of rescanning all children. With `ORDER BY parent.id LIMIT
...`, jed preserves that nested-loop order and stops opening later child bounds as soon as the result
window is full. Each bound it does start is gathered completely, so duplicate child keys retain their
primary-key tie-break and the result is identical to the blocking plan.

## Sort order (`ASC` / `DESC`, `NULLS FIRST` / `NULLS LAST`)

Each key of an ordered index can declare its sort order, as in PostgreSQL:

```sql
CREATE INDEX score_points ON score (points DESC);
CREATE INDEX feed ON post (author, created_at DESC NULLS LAST);
```

The direction defaults to `ASC`, and NULLs default to the largest value — last for `ASC`, **first
for `DESC`** — unless you write `NULLS FIRST` or `NULLS LAST`. The index stores its entries in that
order, so an `ORDER BY` whose every key matches the index (direction *and* NULL placement) walks the
index instead of sorting, and with a `LIMIT` it stops after the first few entries. Below, the
leaderboard reads only the top of `score_points`: the `NULL` score comes first, exactly as
`ORDER BY points DESC` places it. Edit the query to `ORDER BY points DESC NULLS LAST` and the plan
falls back to a sort — a different order, with the same rows a sort always gives.

<LiveSql demo="sortQuery" seed={sortSeed} query={sortQuery} rows={4} />

Equality and range lookups (`points >= 80`), `IN` lists, joins, and `UPDATE`/`DELETE` targets use a
`DESC` index exactly like an `ASC` one, and a `UNIQUE` index enforces the same rule whatever its order.
Two narrowings: jed does not walk an `ASC` index backward to serve a `DESC` order (declare the index
in the order you query), and only ordered B-trees take an order — `USING gin` or `gist` rejects
`ASC`/`DESC`/`NULLS` with `0A000`. As in PostgreSQL, a `UNIQUE (...)` or `PRIMARY KEY (...)`
constraint takes no order; use `CREATE UNIQUE INDEX ... (col DESC)`. [`jed_indexes`](../introspection/)
reports each key's `descending` and `nulls_first` flags.

## Expression indexes

A key element can be an **expression** over the table's columns instead of a bare column — a bare
function call like `lower(email)`, or a parenthesized expression like `(a + b)`. jed then accelerates
a query whose `WHERE` uses that **same expression**: `WHERE lower(email) = …` seeks an index on
`lower(email)`. An index may mix column and expression keys — `CREATE INDEX ON t (kind, lower(name))`.

The canonical use is **case-insensitive uniqueness**: a `UNIQUE` index on `lower(email)` forbids two
rows whose emails differ only in case. Insert a second row with `'ADA@EXAMPLE.COM'` below to see the
`23505` unique violation, then run the case-insensitive lookup:

<LiveSql demo="exprQuery" seed={exprSeed} query={exprQuery} rows={4} />

The expression must be **immutable** — a deterministic function of the row. jed rejects, at
`CREATE INDEX`, an expression that calls a non-immutable function (`now()`, `uuidv4()`, `nextval(…)`)
with `42P17`, an aggregate with `42803`, a window function with `42P20`, or a subquery with `0A000`.
A general operator expression must be parenthesized (`(a + b)`); a bare `a + b` is a syntax error, and
a parenthesized bare column `(a)` is just a column key. The expression's result must be a
key-encodable type. Matching is **syntactic**, as in PostgreSQL: an index on `(a + b)` accelerates
`WHERE a + b = …` but not the re-associated `WHERE b + a = …`.

## Partial indexes (`WHERE`)

A trailing `WHERE predicate` makes the index **partial** — it holds an entry only for the rows
whose predicate is **true**, so a narrow index over a hot subset stays small and is maintained only
for the rows that matter:

```sql
CREATE INDEX orders_active ON orders (customer) WHERE status = 'active';
```

jed uses a partial index to accelerate a query **only when the query's `WHERE` implies the index
predicate** — that is what guarantees every row the query wants is in the index. So
`WHERE status = 'active' AND customer = 10` seeks `orders_active`, while a bare `WHERE customer = 10`
takes a full scan (it can't assume all `customer = 10` rows are active). Run the gated query below,
then edit it to drop `status = 'active'` and watch the plan fall back to a full scan — the rows are
identical either way, because the `WHERE` stays the residual filter.

<LiveSql demo="partialQuery" seed={partialSeed} query={partialQuery} rows={4} />

A **`UNIQUE`** partial index constrains only the qualifying rows: `CREATE UNIQUE INDEX ON orders
(customer) WHERE status = 'active'` forbids two *active* orders from sharing a customer, while a
`cancelled` order may still reuse that customer freely. The predicate is a boolean expression over
the table's own columns and must be **immutable** — jed rejects a non-boolean predicate with
`42804`, and an aggregate / window / subquery / bind-parameter / non-immutable predicate with the
same codes an expression key uses.

The `WHERE` need not restate the predicate word for word. jed proves implication with a bounded
version of PostgreSQL's rules: each `AND`ed part of the predicate must follow from the query's
conditions, and `OR`, `IN (…)`, `= ANY(ARRAY[…])`, and `BETWEEN` are taken apart on either side.
Comparisons with constants of the same type imply each other the way you would expect: an index
`WHERE amount > 0` serves `WHERE amount > 50`, `WHERE amount = 7`, and `WHERE amount IN (3, 9)`, but
not `WHERE amount >= 0`, which also admits `0`. A comparison with a constant also implies
`column IS NOT NULL`. A bind parameter `$1` is never treated as a known value here, so
`WHERE amount > $1` does not use an index `WHERE amount > 0`.

A partial index can also be read **whole**. When the `WHERE` implies the predicate but puts no
condition on the indexed columns, jed may walk the entire (small) index instead of the table, which
also delivers rows in index order. The classic case is a soft-delete index over the few live rows:
`ORDER BY priority LIMIT 2` below walks `task_live` in priority order and stops after two rows, with
no sort. `EXPLAIN` shows the walk as `Index scan: using task_live`.

<LiveSql demo="softQuery" seed={softSeed} query={softQuery} rows={4} />

A partial index also serves an `OR` / `IN` list on its key, the `ORDER BY … LIMIT` walk, and the inner
side of an index nested-loop join. For a join, the join's own `ON` must imply the predicate, or the
predicate must be one that a missing row (`NULL` in every column, as an outer join produces) can never
satisfy. A `LEFT JOIN … WHERE b.deleted_at IS NULL` therefore does not probe an index `WHERE deleted_at
IS NULL`, because that would miss rows.

To use a **partial unique index** as the arbiter of an upsert, restate (or imply) its predicate after
the conflict target, as in PostgreSQL: `INSERT … ON CONFLICT (customer) WHERE status = 'active' DO
UPDATE …`. See [upsert with `ON CONFLICT`](../tables/).

### Timestamps and timezone data

A `timestamptz` column can participate in any admitted immutable expression, including a soft-delete
predicate:

```sql
CREATE TABLE accounts (id int PRIMARY KEY, email text, deleted_at timestamptz);
CREATE UNIQUE INDEX active_email ON accounts (email) WHERE deleted_at IS NULL;
```

<LiveSql demo="nullQuery" seed={nullSeed} query={nullQuery} rows={4} />

Instant comparisons and explicit-zone conversions are allowed. A cast such as `deleted_at::date`
reads the session timezone and is rejected with `42P17`; `(deleted_at AT TIME ZONE 'UTC')::date`
is safe. A `timestamptz` literal needs an explicit offset for the same reason:
`WHERE deleted_at > '2024-01-01 00:00+00'` is accepted, while `'2024-01-01 00:00'` (read in the session
timezone) is `42P17`. The same rules compose through CASE, COALESCE, arrays, and nested function calls.

Named zones must be loaded when the index is created. jed stores their version and checksum with
the index; a computed zone argument pins the entire loaded zone set. If the loaded data differs,
reads skip that index and writes to its table fail with `XX002`. After loading the desired timezone
bundle in a fresh process, rebuild the affected index with [`REINDEX`](#reindex):

```sql
REINDEX INDEX local_day;
```

The rebuild pins the zone data now loaded. If it fails, nothing changes and the old index keeps its
pins. These dependency records use
file format 32; plain timestamp values and column indexes require no timezone data.

## GIN indexes for arrays (`USING gin`)

A **GIN** (generalized inverted) index maps the **elements** of an array column to the rows that
contain them, so a query over a multi-valued column narrows to candidate rows instead of reading
the whole table. Add one with `USING gin`:

```sql
CREATE INDEX post_tags_gin ON post USING gin (tags)
```

It accelerates the two array set operators, array membership, and exact equality:

- **`tags @> ARRAY[10, 20]`** (contains) — rows whose `tags` contain **all** the query terms. jed
  gathers the rows for each term and **intersects** their lists.
- **`tags && ARRAY[30, 40]`** (overlaps) — rows whose `tags` share **any** query term. jed gathers
  the lists and takes their **union**.
- **`20 = ANY(tags)`** (membership) — rows that have `20` among their `tags` (the array spelling of
  membership; equivalently `tags @> ARRAY[20]`). jed gathers that single term's rows.
- **`tags = ARRAY[10, 20]`** (equality) — rows whose `tags` **exactly** equal the query array. Since
  equal arrays contain the same elements, jed gathers the same candidates as `@>` and then the
  residual `=` enforces order and length — **stricter** than containment.

An **`IN` list** over the column (`tags IN (ARRAY[1, 2], ARRAY[3])`), or any **`OR`** whose every
arm is one of these on the same column (`tags @> ARRAY[3] OR 7 = ANY(tags)`), uses the index too:
jed gathers each arm's rows and takes their union, as PostgreSQL's bitmap OR does. An `OR` with an
arm the index cannot serve (`tags @> ARRAY[3] OR n = 5`) is a full scan.

The original `WHERE` stays as the residual filter, so the answer is identical to the full-scan
answer — the index is transparent. The same bound applies to **`UPDATE` and `DELETE`**: a mutation
whose `WHERE` is GIN-accelerable narrows its target-row scan through the index too, so the rows it
rewrites or removes are exactly the full-scan set (only faster).

For a one-table `SELECT`, an eligible GIN path competes by deterministic estimated cost with the
full scan, primary-key, B-tree, GiST, and interval alternatives. The estimate covers the complete
pipeline and may therefore choose a different path as row count, residual selectivity, ordering, or
LIMIT changes; exact ties follow a fixed access-kind and lowercased-name order.

For a bounded `SELECT ... LIMIT`, jed completes the posting-list gather, then fetches and rechecks
candidate table rows only until the requested window is full. With an ordered B-tree bound, it can
stop the index walk itself at the same point. A matching `ORDER BY` keeps this bounded path; an
incompatible order still consumes and sorts the complete candidate set.

Containment (`intro` and `gin` both hold `{10, 20}`):

<LiveSql demo="containsQuery" seed={ginSeed} query={containsQuery} rows={6} />

Overlap (`intro` holds `30`; `arrays` and `storage` hold `40`):

<LiveSql demo="overlapsQuery" seed={ginSeed} query={overlapsQuery} rows={6} />

Membership (`intro`, `arrays`, and `gin` all hold `20`):

<LiveSql demo="memberQuery" seed={ginSeed} query={memberQuery} rows={6} />

Equality is stricter than containment — `tags = ARRAY[10, 20]` keeps only `gin` (whose `tags`
*are* `{10, 20}`), not `intro` (whose `{10, 20, 30}` merely *contains* them):

<LiveSql demo="equalQuery" seed={ginSeed} query={equalQuery} rows={6} />

## GIN indexes for `jsonb`

A GIN index on a `jsonb` column indexes every **key** and every **scalar value** of each
document, at every nesting level — PostgreSQL's default `jsonb_ops` operator class. It accelerates
containment and key existence:

- **`body @> '{"kind": "login"}'`** (contains; `'…' <@ body` is the same) — documents that contain
  the query document. jed gathers the rows carrying *all* of the query's keys and values.
- **`body ? 'tags'`** — documents with that top-level key (or, for an array, that string element).
- **`body ?| '{a,b}'`** / **`body ?& '{a,b}'`** — any / all of the keys.

```sql
CREATE INDEX event_body_gin ON event USING gin (body)
```

The index ignores where in the document a key or value sits, so the rows it gathers are candidates
that the original operator always rechecks — the answer is exactly the full-scan answer.
Containment follows PostgreSQL: a nested value must match in kind, so `{"tags": ["beta"]}` does not
contain `{"tags": "beta"}`, and only a top-level array contains a bare scalar element. `UPDATE` and
`DELETE` use the index for their target rows, as they do for arrays. A `json` column has no GIN
operator class (`42704`).

<LiveSql demo="jsonbContainsQuery" seed={jsonbSeed} query={jsonbContainsQuery} rows={6} />

Key existence (`1` and `4` have a top-level `tags` key):

<LiveSql demo="jsonbKeyQuery" seed={jsonbSeed} query={jsonbKeyQuery} rows={6} />

A key or string longer than 128 bytes is indexed by its first 128 bytes, so very long values that
share a prefix also share an index entry; the recheck tells them apart. The jsonpath operators
`@?` and `@@` are not accelerated yet, and run by full scan.

### Current scope

GIN this release covers a focused surface (it grows from here):

- **A `jsonb` column** — the `jsonb_ops` operator class above, for `@>`, `<@`, `?`, `?|`, and `?&`.

- **Or one column, an array** of the integers (`i16[]`, `i32[]`, `i64[]`), `text[]`, `bytea[]`,
  `numeric[]`, `uuid[]`, `date[]`, `timestamp[]`, `timestamptz[]`, `boolean[]`, `interval[]`, or
  the binary floats (`f32[]`, `f64[]`). A GIN term *is* the element's key encoding. Arrays of
  composite or `jsonb` elements are rejected (`0A000`) for now, as is a multi-column GIN.
- **Text elements match byte for byte** — `'Red' = ANY(tags)` does not find `{red}`, under any
  collation, as with text `=`. A `text[]` column with a `COLLATE` clause indexes the same way. An
  element longer than 128 bytes is indexed by its first 128 bytes, so very long elements that share
  a prefix share an index entry; the recheck tells them apart, and a long element never makes its
  row unindexable.
- **Numeric elements match by value** — `1.5`, `1.50`, and `1.500` are one term, so
  `1.5 = ANY(prices)` finds a stored `{1.50}`. An integer works against a `numeric[]` column
  (`2 = ANY(prices)` finds `{2.0}`), and a numeric against an integer array (`2.0 = ANY(ids)` finds
  `{2}`, while `2.5` matches nothing). `bytea` elements match byte for byte. As with text, an element
  whose key is longer than 128 bytes is indexed by its first 128 bytes and rechecked.
- **Float elements match like float `=`** — `-0` and `0` are one term and every `NaN` is one term
  (PostgreSQL's float order: `-0 = 0`, `NaN = NaN`), so `0 = ANY(col)` finds a stored `{-0}` and
  `'NaN'::f64 = ANY(col)` finds a stored `{NaN}`. A query value of the other float width works too:
  `'1.5'::f64 = ANY(f32_col)` uses the index, while an `f64` no `f32` can represent (such as
  `0.1`) matches nothing, exactly as the full scan would.
- **Interval elements match like interval `=`** — intervals compare by their total span (a month
  counts as 30 days, a day as 24 hours), so `1 mon`, `30 days`, and `720:00:00` are one term:
  `'1 mon'::interval = ANY(col)` finds a stored `{30 days}`, exactly as the full scan would. The
  stored values keep their own spelling.
- **For arrays, `@>`, `&&`, `= ANY`, and array `=`**, and `IN` lists or `OR`s of them — `<@`
  (contained-by) still runs by full scan; it is not GIN-accelerated yet.
- **No `UNIQUE`** — an inverted index has many entries per row, so `CREATE UNIQUE INDEX … USING gin`
  is rejected (`0A000`), matching PostgreSQL.

`DROP INDEX`, auto-naming, and the `DROP TABLE` cascade work the same as for an ordered index.

<a id="reindex"></a>

## Rebuilding indexes (`REINDEX`)

`REINDEX` rebuilds indexes from their stored definitions, as if each were dropped and created again
with the same name, keys, uniqueness, and predicate — in one all-or-nothing statement that keeps the
constraints an index backs. It never changes a query's answer:

```sql
REINDEX INDEX city_region;     -- one index
REINDEX TABLE city;            -- every index of a table
REINDEX DATABASE;              -- every index in the database (and your temporary tables)
```

<LiveSql demo="reindexQuery" seed={orderedSeed} query={reindexQuery} rows={3} />

Its main use is to make an index usable again after something it depends on changed: a
[host function](../../api/host-functions/) with a new version, or the timezone data an index pinned
(above). Until then reads skip such an index and writes to its table fail with `XX002`. `REINDEX`
re-resolves the key and predicate against what is loaded now, records the new dependencies, and
rebuilds the keys; a `UNIQUE` index re-checks its rows (`23505`). If any index fails, none changes.
An index whose stored definition this version of jed cannot parse (`XX003`) can only be dropped.

The cost is exactly what `CREATE INDEX` of each rebuilt index would charge. `REINDEX INDEX` and
`REINDEX TABLE` run inside a transaction and roll back with it; as in PostgreSQL, `REINDEX DATABASE`,
`REINDEX SYSTEM`, and the `CONCURRENTLY` form cannot run inside a transaction block (`25001`).
`CONCURRENTLY` otherwise rebuilds the same way — readers never wait for the writer in jed — and the
`(VERBOSE)` option prints nothing. `REINDEX SYSTEM` has nothing to rebuild (jed has no system-catalog
indexes), and `REINDEX SCHEMA` and the `TABLESPACE` option are not supported (`0A000`). A session
needs DDL permission, and `SELECT` on the table for the `INDEX` and `TABLE` forms.

