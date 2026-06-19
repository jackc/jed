<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TABLE account (
  id      i32 PRIMARY KEY,
  owner   text NOT NULL,
  balance numeric(12,2) NOT NULL CHECK (balance >= 0)
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
	<meta name="description" content="CREATE TABLE with typed columns, PRIMARY KEY, NOT NULL, CHECK, UNIQUE and FOREIGN KEY constraints — enforced live." />
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
