<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TABLE account (
  id      i32 PRIMARY KEY,
  owner   text NOT NULL,
  balance numeric(12,2) NOT NULL,
  CONSTRAINT balance_nonnegative CHECK (balance >= 0)
);
INSERT INTO account VALUES (1, 'Ada', 100.00), (2, 'Grace', 50.00);
CREATE TABLE txn (
  id         i32 PRIMARY KEY,
  account_id i32 NOT NULL REFERENCES account ON DELETE CASCADE ON UPDATE CASCADE,
  amount     numeric(12,2) NOT NULL
);
INSERT INTO txn VALUES (1, 1, 25.00), (2, 2, 10.00);`;

	const query = `SELECT id, owner, balance FROM account ORDER BY balance DESC;`;

	const unicodeEscapeDemo = `SELECT U&'caf\\00e9' AS U&"n\\00e4me",
       U&'d!0061t!+000061' UESCAPE '!' AS decoded;`;
</script>

<svelte:head>
	<title>Tables &amp; constraints — jed</title>
	<meta name="description" content="Create and alter typed tables — from column definitions or from a query with CREATE TABLE AS — with serial and identity auto-numbering, defaults, and enforced PRIMARY KEY, NOT NULL, CHECK, UNIQUE and FOREIGN KEY constraints." />
</svelte:head>

# Tables & constraints

`CREATE TABLE` declares typed columns and constraints: `PRIMARY KEY`, `NOT NULL`, `DEFAULT`,
`CHECK`, `UNIQUE`, and `FOREIGN KEY`. Constraints are enforced on every write, with a structured
error code when violated.

Two tables below: `account` (with a `CHECK (balance >= 0)` and a `NOT NULL` owner) and `txn`, whose
`account_id` is a `FOREIGN KEY` that `REFERENCES account`. Run the query, then try editing it to
break a constraint:

<LiveSql demo="accountDemo" {seed} {query} rows={6} />

Things to try in the panel above:

- **CHECK** — `INSERT INTO account VALUES (3, 'Bob', -5);` &rarr; error `23514`
- **PRIMARY KEY** uniqueness — `INSERT INTO account VALUES (1, 'Dup', 1);` &rarr; error `23505`
- **NOT NULL** — `INSERT INTO account VALUES (4, NULL, 1);` &rarr; error `23502`
- **FOREIGN KEY** — `INSERT INTO txn VALUES (3, 99, 5);` &rarr; error `23503` (no account `99`)
- **FOREIGN KEY action** — `DELETE FROM account WHERE id = 1; SELECT * FROM txn;` &rarr; the
  referencing txn row is deleted by `ON DELETE CASCADE`

Each constraint violation is rejected before anything is written — a statement is all-or-nothing.
See the [error reference](../../reference/errors/) for every code.

Creating a table whose name is already taken by a table, index, or sequence is error `42P07`.
`CREATE TABLE IF NOT EXISTS` makes it a no-op instead, so setup scripts can be re-run safely. It
does not compare definitions: the existing table keeps its columns and rows, even if the new
definition differs.

```sql
CREATE TABLE IF NOT EXISTS account (id i32 PRIMARY KEY, owner text);   -- no-op: account exists
```

Foreign keys accept `ON DELETE` and `ON UPDATE` with `NO ACTION` (the default), `RESTRICT`,
`CASCADE`, `SET NULL`, or `SET DEFAULT`. Actions recurse through dependent tables. `SET DEFAULT`
uses each referencing column's declared default, or `NULL` when it has none; generated changes run
the same type, `NOT NULL`, `CHECK`, uniqueness, and foreign-key checks as an ordinary statement.

Deleting or re-keying a parent row checks every referencing table for rows that still point at it,
and an action such as `ON DELETE CASCADE` finds the rows it changes the same way. As in PostgreSQL,
jed does not index a foreign key's referencing columns for you. Without such an index, each check
scans the whole child table, and each action scans it once per statement. Create an index that
starts with the foreign-key columns (or make them the leading primary-key columns), and each check
or action becomes one index lookup per parent row. This work counts toward a session's `max_cost`.

A multi-column foreign key with a `NULL` in any referencing column is not checked: that row
references nothing (`MATCH SIMPLE`, the default). Write `MATCH FULL` after the referenced columns to
allow only all-`NULL` or all-non-`NULL` keys; a row that mixes them is error `23503`.
`MATCH PARTIAL` is not supported (`0A000`), as in PostgreSQL.

```sql
CREATE TABLE shipment (
  id i32 PRIMARY KEY, region text, code i32,
  FOREIGN KEY (region, code) REFERENCES depot (region, code) MATCH FULL ON DELETE SET NULL
);
INSERT INTO shipment VALUES (1, NULL, NULL);   -- ok: references nothing
INSERT INTO shipment VALUES (2, 'west', NULL); -- error 23503 under MATCH FULL
```

## Creating a table from a query

`CREATE TABLE … AS query` creates a table from a query's result and fills it with the query's rows
in the same statement, as in PostgreSQL. Each column takes the query column's name and type. A
column copied straight from a table keeps its `numeric(p,s)` or `varchar(n)` limit; a computed
column such as `balance * 2` does not. Constraints, defaults, and keys are not copied. Try it in the
panel above:

```sql
CREATE TABLE big_spender AS SELECT owner, balance FROM account WHERE balance > 60;
INSERT INTO big_spender VALUES ('Eve', 1.234);    -- balance keeps numeric(12,2): stored 1.23
SELECT owner, balance FROM big_spender ORDER BY owner;
```

A column list after the table name renames the columns, left to right. `WITH NO DATA` at the end
creates the table empty without running the query. The query can be any query: joins, aggregates,
`UNION`, `VALUES`, or a `WITH` clause.

```sql
CREATE TABLE totals (owner, spent) AS
  SELECT a.owner, sum(t.amount) FROM account a JOIN txn t ON t.account_id = a.id GROUP BY a.owner;
CREATE TABLE account_shape AS SELECT * FROM account WITH NO DATA;   -- same columns, no rows
```

The query is checked before the table name, so `CREATE TABLE IF NOT EXISTS … AS` still reports an
error in the query even when the table exists. A query that reads tables needs the `SELECT`
privilege on them, unless `WITH NO DATA` is used. A data-modifying `WITH` in the query is not
supported yet (`0A000`). See [temporary tables](../temp-tables/) for `CREATE TEMP TABLE … AS`.

## Names and quoted identifiers

Names follow PostgreSQL's rules. A bare name is folded to lowercase, so `Orders`, `ORDERS`, and
`orders` are the same table. A name in double quotes keeps its exact spelling, so it can hold
uppercase letters, spaces, or a keyword such as `order`. Write a literal `"` inside one by doubling
it. After folding, names match exactly: `"userId"` must be written with quotes everywhere you use
it, and `"Orders"` is a different table from `orders`.

```sql
CREATE TABLE Orders (Id i32 PRIMARY KEY, "userId" i32, "order" text);
SELECT id, "userId", "order" FROM orders;   -- the table is orders, the column id
SELECT userId FROM orders;                  -- error 42703: userid does not exist
SELECT 1 AS Total, 2 AS "Total";            -- columns named total and Total
```

Built-in function and type names are lowercase, so `LOWER(x)` works but `"LOWER"(x)` does not. The
name passed to `nextval('…')` is read the same way: `nextval('MySeq')` means `myseq`, and
`nextval('"MySeq"')` means `MySeq`. A name may be at most 63 bytes (error `42622`). Names jed
makes up for unnamed constraints, indexes, and serial sequences (`orders_pkey`, `orders_email_key`,
`orders_id_seq`) are shortened to fit the same 63 bytes, exactly as PostgreSQL shortens them, so you
can always refer to them. If another constraint in the same database already has the name jed
would make up, a number is added (`orders_email_key1`), as in PostgreSQL. Temporary tables and each
attached database count as separate databases for this. Names that start with `jed_` are reserved for jed's own catalog relations
(error `42939`).

A quoted name or a string literal can also spell characters by code point, as in PostgreSQL. Put
`U&` before the opening quote, then write `\XXXX` (4 hex digits) or `\+XXXXXX` (6 hex digits) for a
character and `\\` for a backslash. Add `UESCAPE '!'` after the literal to use another escape
character. A malformed escape is error `42601`.

<LiveSql demo="unicodeEscapeDemo" query={unicodeEscapeDemo} rows={1} />

## Applying defaults

Omitting a column or using `DEFAULT` in a VALUES slot applies its declared default. To insert one
row using defaults for every column, use `DEFAULT VALUES`; to reset an existing value, assign
`DEFAULT` in an UPDATE — or in the `DO UPDATE` action of an upsert (see below):

```sql
CREATE TABLE job (
  id i32 PRIMARY KEY DEFAULT 1,
  state text NOT NULL DEFAULT 'queued',
  attempts i32 DEFAULT 0
);
INSERT INTO job DEFAULT VALUES;
UPDATE job SET state = DEFAULT, attempts = DEFAULT WHERE id = 1;
INSERT INTO job (id, state) VALUES (1, 'retry')
  ON CONFLICT (id) DO UPDATE SET state = excluded.state, attempts = DEFAULT;
```

A column with no declared default takes `NULL`, so the ordinary `NOT NULL` check still applies.
Expression defaults are evaluated separately for each inserted or updated row; in an upsert, a
`DEFAULT` assignment is evaluated only for rows the `DO UPDATE` actually updates, so a sequence-backed
default advances only for those.

## Expressions in VALUES

A `VALUES` slot can hold any expression: arithmetic, function calls, `CASE`, casts, and scalar
subqueries.

```sql
INSERT INTO account VALUES
  ((SELECT max(id) FROM account) + 1, upper('linus'), 20 * 2.50);
```

An expression has no columns in scope, and its result must be assignable to the target column
(error `42804` otherwise). A subquery runs once, against the table as it was before the statement.
Every expression is evaluated before any row is written, so an error such as division by zero
stores nothing.

## Altering a table

`ALTER TABLE` can add or drop columns, rename a table, column, or constraint; change a column's
type, default, or `NOT NULL` status; add or drop CHECK, UNIQUE, FOREIGN KEY, and EXCLUDE constraints;
and add or drop a primary key.
Try these statements:

```sql
ALTER TABLE account RENAME COLUMN balance TO available_balance;
ALTER TABLE account ALTER COLUMN available_balance SET DEFAULT 0;
ALTER TABLE account ALTER COLUMN available_balance SET NOT NULL;
ALTER TABLE account ADD COLUMN opened_by text DEFAULT 'import';
ALTER TABLE account ALTER COLUMN balance TYPE i64 USING balance + 0;
ALTER TABLE account DROP COLUMN opened_by;
ALTER TABLE account RENAME CONSTRAINT balance_nonnegative TO nonnegative_balance;
ALTER TABLE account ADD CONSTRAINT owner_unique UNIQUE (owner);
ALTER TABLE account DROP CONSTRAINT owner_unique;
ALTER TABLE account DROP PRIMARY KEY;
ALTER TABLE account ADD PRIMARY KEY (id);
ALTER TABLE account RENAME TO ledger_account;
```

Column actions can be combined with commas. They run from left to right and publish only if every
action succeeds:

```sql
ALTER TABLE account
  ALTER COLUMN balance SET DEFAULT 0,
  ALTER COLUMN balance SET NOT NULL;
```

`SET DEFAULT` affects future inserts, while `SET NOT NULL` first checks every existing row and
reports `23502` if any value is null. Renaming a column also updates its stored `CHECK` and default
expressions, plus expression and partial-index definitions. `IF EXISTS` is supported for a missing
table.

Adding a constraint validates existing rows before publishing it and builds the backing index for
UNIQUE or EXCLUDE. Dropping a referenced UNIQUE constraint defaults to `RESTRICT`; `CASCADE` also
removes dependent foreign keys. `ADD COLUMN` rewrites existing rows with the column's default (or
null), evaluates expression defaults once per row, and accepts the same inline constraints as
`CREATE TABLE`; `IF NOT EXISTS` is available. A non-empty table cannot add a `NOT NULL` column with
no usable default (`23502`). `DROP COLUMN` also rewrites the table, physically removes the row slot,
and compacts surviving ordinals. As in PostgreSQL, same-table CHECK, index (including UNIQUE), and
EXCLUDE dependents are removed automatically, as is a foreign key that owns the column on its local
side. The default `RESTRICT` mode still reports `2BP01` for external dependents such as a foreign key
that references the column from another table; `CASCADE` removes those too. `ALTER COLUMN TYPE`
converts every row through jed's explicit cast matrix, or through a per-row `USING` expression, then
revalidates constraints and rebuilds indexes. `ADD PRIMARY KEY` validates NOT NULL and uniqueness
before re-keying; `DROP PRIMARY KEY` re-keys to synthetic rowids and retains member NOT NULL status.
ALTER TABLE does not yet manage identity properties. Identity-column defaults and nullability must be
managed through the future
identity-specific syntax rather than the generic column actions. PRIMARY KEY has no persisted constraint
object, so renaming the derived `account_pkey` handle reports `42704`; an explicit name on an added
primary key is accepted but the handle remains derived.

## Exclusion constraints — `EXCLUDE`

An `EXCLUDE` constraint generalizes `UNIQUE`: instead of forbidding two rows with the *equal* key,
it forbids two rows that make a list of comparisons all true at once. The classic use is
no-double-booking — no two reservations may share a room **and** have overlapping time ranges. Paste
this into the panel above:

```sql
CREATE TABLE booking (
  id i32 PRIMARY KEY,
  room i32,
  during i32range,
  EXCLUDE USING gist (room WITH =, during WITH &&)
);
INSERT INTO booking VALUES (1, 101, '[10,20)');
INSERT INTO booking VALUES (2, 101, '[15,25)');  -- error 23P01: same room, overlapping time
```

The second insert is rejected with `23P01` — room `101` is already booked for an overlapping
range. A different room, or a non-overlapping range, is fine. The supported operators are `=` (over a
fixed-width scalar) and `&&` (range overlap); `UNIQUE` is the special all-`=` case. A row with `NULL`
in an excluded column — or an empty range under `&&` — never conflicts (it is exempt). The constraint
is backed by a GiST index and enforced on every write, all-or-nothing like the others. (PostgreSQL
needs `CREATE EXTENSION btree_gist` for the scalar `=` member; jed ships it in-core.)

Rescheduling is just an `UPDATE` — assigning a range (or array) column re-checks every constraint
over the statement's end state, so moving a booking to a free slot succeeds and one that would
overlap a *different* same-room booking is rejected:

```sql
INSERT INTO booking VALUES (2, 101, '[30,40)');      -- a second slot in room 101
UPDATE booking SET during = '[20,28)' WHERE id = 1;  -- ok: still no overlap
UPDATE booking SET during = '[35,45)' WHERE id = 1;  -- error 23P01: now overlaps booking 2
```

The range literal on the right adapts to the column's type; an `i32range(20,28)` constructor, a cast,
or a `during + '[5,8)'::i32range` expression work too.

## Auto-numbering with `serial`

A `serial` column (or `bigserial` / `smallserial` for `i64` / `i16`) is shorthand for an
auto-numbering integer: it creates a dedicated sequence and defaults the column to that sequence's
next value, and the column is `NOT NULL`. Omit it on insert and it fills in `1`, `2`, … Paste this
into the panel above:

```sql
CREATE TABLE log (id serial PRIMARY KEY, msg text);
INSERT INTO log (msg) VALUES ('first'), ('second');
SELECT * FROM log;
```

`id` is assigned `1` then `2` automatically. The owned sequence is named `log_id_seq`; `DROP TABLE
log` drops it too. (Supplying an explicit `id` overrides the default for that row without advancing
the sequence — just like PostgreSQL.)

The backing sequence matches the column's type: `smallserial` / `bigserial` (and a `smallint` / `bigint`
identity column) get a sequence bounded to that integer type's range, so it tops out exactly where the
column does — a `smallserial` sequence stops at `32767`. A standalone `CREATE SEQUENCE` can choose the
type the same way with `AS smallint | integer | bigint` (default `bigint`).

## Identity columns — `GENERATED … AS IDENTITY`

The SQL-standard spelling of an auto-numbered column. Like `serial` it creates an owned sequence and
fills the column in, but it also records whether values may be supplied by hand:

```sql
CREATE TABLE event (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, label text);
INSERT INTO event (label) VALUES ('login'), ('logout');
SELECT * FROM event;
```

- **`GENERATED ALWAYS`** — the value always comes from the sequence. Supplying an explicit value is an
  error (`428C9`) unless you ask for it with `OVERRIDING SYSTEM VALUE`:

  ```sql
  INSERT INTO event (id, label) OVERRIDING SYSTEM VALUE VALUES (100, 'imported');
  ```

- **`GENERATED BY DEFAULT`** — like `serial`: an explicit value is used when given (and does not
  advance the sequence), otherwise the sequence fills in. `OVERRIDING USER VALUE` forces the sequence
  even when a value is supplied.

The column must be `smallint`, `integer`, or `bigint`, and is implicitly `NOT NULL`. You can tune the
backing sequence inline — `GENERATED ALWAYS AS IDENTITY (START WITH 100 INCREMENT BY 5)` — and, as
with `serial`, the owned sequence is named `event_id_seq` and is dropped with the table.

## Upsert with `ON CONFLICT`

Instead of trapping `23505` when an insert collides with a `PRIMARY KEY` or `UNIQUE` constraint, an
`ON CONFLICT` clause takes a controlled action. **`DO NOTHING`** skips the offending row;
**`DO UPDATE`** updates the existing conflicting row — the row you tried to insert is available as the
special `excluded` relation, while a bare or table-qualified column reads the existing row. Run this
in the panel above (after the `account` table exists):

```sql
INSERT INTO account VALUES (1, 'Ada', 100.00)
  ON CONFLICT (id) DO UPDATE SET balance = account.balance + excluded.balance;
SELECT id, owner, balance FROM account WHERE id = 1;
```

Account `1` already exists, so instead of erroring the row is updated — its balance becomes
`100.00 + 100.00`. The parenthesised **conflict target** `(id)` names which unique constraint to
arbitrate on (matched by column set; you can also write `ON CONSTRAINT account_pkey`). More to try:

- **`DO NOTHING`** — `INSERT INTO account VALUES (1, 'Dup', 1) ON CONFLICT DO NOTHING;` succeeds and
  changes nothing (with no target it skips a conflict on *any* unique constraint).
- **A filtered update** — add `WHERE excluded.balance > account.balance` to a `DO UPDATE` to apply it
  only when the proposed balance is larger; otherwise the row is left untouched.
- **`RETURNING`** — append `RETURNING id, balance` to see the affected (inserted or updated) rows.
- **Resetting to a default** — `DO UPDATE SET col = DEFAULT` assigns the column's declared default
  (or `NULL`), exactly like `UPDATE … SET col = DEFAULT` (see Applying defaults above).
- **Array, range, and composite columns** — `DO UPDATE` assigns them as `UPDATE` does: `NULL`, a
  text literal (`'{1,2}'`, `'[1,5)'`, `'(Main,1)'`), or a value of the column's own type such as
  `excluded.col`.
- **A target alias** — `INSERT INTO account AS a VALUES (1, 'Ada', 5.00) ON CONFLICT (id) DO UPDATE
  SET balance = a.balance + excluded.balance RETURNING a.balance;` names the existing row `a`. Once
  aliased, the table is referred to only by its alias (`account.balance` would be `42P01`), in the
  `DO UPDATE` and in `RETURNING` alike.

`RETURNING` can also compare row versions. `old.balance` is the value before the change and
`new.balance` the value after it; an `INSERT` has an all-`NULL` old row and a `DELETE` an all-`NULL`
new row. Use aliases when the names would be clearer—or when the target table itself is named
`old` or `new`:

```sql
UPDATE account SET balance = balance + 25
  WHERE id = 1
  RETURNING WITH (OLD AS before, NEW AS after) before.balance, after.balance;
```

A conflict on a constraint *other* than the arbiter still raises `23505`, and a single statement that
would update the same existing row twice raises `21000`. The whole statement is all-or-nothing.
