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
  account_id i32 NOT NULL REFERENCES account,
  amount     numeric(12,2) NOT NULL
);
INSERT INTO txn VALUES (1, 1, 25.00), (2, 2, 10.00);`;

	const query = `SELECT id, owner, balance FROM account ORDER BY balance DESC;`;
</script>

<svelte:head>
	<title>Tables &amp; constraints — jed</title>
	<meta name="description" content="Create and alter typed tables with serial and identity auto-numbering, defaults, and enforced PRIMARY KEY, NOT NULL, CHECK, UNIQUE and FOREIGN KEY constraints." />
</svelte:head>

# Tables & constraints

`CREATE TABLE` declares typed columns and constraints: `PRIMARY KEY`, `NOT NULL`, `DEFAULT`,
`CHECK`, `UNIQUE`, and `FOREIGN KEY`. Constraints are enforced on every write, with a structured
error code when violated.

Two tables below: `account` (with a `CHECK (balance >= 0)` and a `NOT NULL` owner) and `txn`, whose
`account_id` is a `FOREIGN KEY` that `REFERENCES account`. Run the query, then try editing it to
break a constraint:

<LiveSql {seed} {query} rows={6} />

Things to try in the panel above:

- **CHECK** — `INSERT INTO account VALUES (3, 'Bob', -5);` &rarr; error `23514`
- **PRIMARY KEY** uniqueness — `INSERT INTO account VALUES (1, 'Dup', 1);` &rarr; error `23505`
- **NOT NULL** — `INSERT INTO account VALUES (4, NULL, 1);` &rarr; error `23502`
- **FOREIGN KEY** — `INSERT INTO txn VALUES (3, 99, 5);` &rarr; error `23503` (no account `99`)
- **FOREIGN KEY** (parent side) — `DELETE FROM account WHERE id = 1;` &rarr; error `23503` (txn `1`
  still references it)

Each is rejected before anything is written — a statement is all-or-nothing. See the
[error reference](../../reference/errors/) for every code.

## Altering a table

`ALTER TABLE` can add columns, rename a table, column, or constraint; change a column's default or
`NOT NULL` status; and add or drop CHECK, UNIQUE, FOREIGN KEY, and EXCLUDE constraints. Try these
statements:

```sql
ALTER TABLE account RENAME COLUMN balance TO available_balance;
ALTER TABLE account ALTER COLUMN available_balance SET DEFAULT 0;
ALTER TABLE account ALTER COLUMN available_balance SET NOT NULL;
ALTER TABLE account ADD COLUMN opened_by text DEFAULT 'import';
ALTER TABLE account RENAME CONSTRAINT balance_nonnegative TO nonnegative_balance;
ALTER TABLE account ADD CONSTRAINT owner_unique UNIQUE (owner);
ALTER TABLE account DROP CONSTRAINT owner_unique;
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
no usable default (`23502`). ALTER TABLE does not yet drop columns, change existing column types, or
manage identity properties. Identity-column defaults and nullability must be managed through the
future identity-specific syntax rather than the generic column actions. PRIMARY KEY has no persisted
constraint object yet, so renaming the derived `account_pkey` handle reports `42704`; standalone
primary-key alteration remains deferred.

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

A conflict on a constraint *other* than the arbiter still raises `23505`, and a single statement that
would update the same existing row twice raises `21000`. The whole statement is all-or-nothing.
