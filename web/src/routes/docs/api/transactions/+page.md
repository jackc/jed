<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Transactions — jed</title>
	<meta name="description" content="Atomic transactions in jed from Rust, Go, or TypeScript." />
</svelte:head>

# Transactions

jed has a **single writer**: at most one write transaction at a time, with readers never blocked
except during the brief commit. A transaction's changes apply all-or-nothing.

The `update` helper runs a read-write transaction that commits on success and rolls back if your code
signals an error — the safest default; there's a read-only `view` helper too. Both are available in
every language (Rust, Go, **and** TypeScript), on the `Database` handle (where each call mints a fresh
session) or on a session you've minted. For finer control, the explicit `begin` / `commit` /
`rollback` form lives on a **session** — mint one with `db.session(...)` and drive the block across
calls. In TypeScript, await each operation: `await db.update(async (tx) => { ... })`,
`await db.session(...)`, and `await session.begin()`. Use the transaction proxy returned by
`begin()` throughout the block; it retains ownership across application awaits.

The closure owns the transaction's lifetime: issuing `COMMIT` or `ROLLBACK` from inside it (as SQL,
or by calling `tx.commit()` / `tx.rollback()`) is refused with `2D000`, and the SQL form fails the
block like any other error. If your closure swallows a statement error and returns normally,
`update` reports `25P02` instead of pretending to commit — a failed transaction commits nothing.
Savepoints (below) work inside the closure as usual. (The async TypeScript runtime drives the block
through the session and does not yet enforce this: a `COMMIT` inside its closure commits through the
session, and later statements autocommit.)

A writable transaction that makes no persistent changes commits without rewriting the database or
issuing an fsync. This includes empty transactions, SELECT-only work, and zero-row writes without
other effects. Temporary-only changes stay in the session; attachment-only changes persist only the
changed attached database. A query that advances a sequence still writes, even if it returns no rows.
The read-only `view` helper remains useful for enforcing access restrictions and avoiding writer contention.

<CodeTabs topic="transactions" />


## Savepoints

Inside a transaction, savepoints give you nested rollback with PostgreSQL's semantics. Run them as
ordinary SQL on the open transaction — there are no extra API methods:

```sql
BEGIN;
INSERT INTO accounts VALUES (1, 100);
SAVEPOINT before_bonus;
INSERT INTO accounts VALUES (2, 50);
ROLLBACK TO SAVEPOINT before_bonus;   -- undoes the second insert only
RELEASE SAVEPOINT before_bonus;       -- forgets the savepoint, keeps the work
COMMIT;                               -- commits account 1
```

`ROLLBACK TO` undoes everything since the savepoint — rows, schema changes, sequence advances,
temporary tables, and writes to attached databases — and keeps the savepoint so you can roll back to
it again. It is also how a transaction recovers after an error: once a statement fails, every other
statement is rejected with `25P02` until you `ROLLBACK` or `ROLLBACK TO` a savepoint taken before the
error. Savepoint names may repeat; `RELEASE` and `ROLLBACK TO` use the most recent one.

A savepoint statement outside a transaction is `25P01`, and a name with no savepoint is `3B001`.
Because sequences in jed are transactional, a value handed out after a savepoint is handed out again
after `ROLLBACK TO` (PostgreSQL would skip it).

## Isolation

Readers see the last committed state and run without blocking against an in-flight writer; the only
exclusive moment is the commit itself. This is **not** MVCC — there is exactly one committed version
plus the current writer's pending changes. It keeps the model simple and the read path nearly
lock-free.

## Durability

When a file-backed commit returns, its changes are on stable storage and survive a crash or a
sudden power loss. Each commit costs one flush, and its cost depends on the platform:

- **Linux:** a commit costs about as much as SQLite's `synchronous=FULL` or a PostgreSQL commit.
  All three use the same flush.
- **macOS:** jed flushes with `F_FULLFSYNC`, which forces the drive to write its own cache to flash.
  That takes about 4 ms per commit on Apple SSDs. SQLite and PostgreSQL, in their default
  configurations, use plain `fsync` on macOS. It returns sooner because the data can still sit in
  the drive's volatile cache. If the machine loses power at the wrong moment, they can lose
  recently committed transactions or be left with a corrupt file. jed accepts the cost to keep that
  guarantee. For a matching comparison, set SQLite's `PRAGMA fullfsync = ON` or PostgreSQL's
  `wal_sync_method = fsync_writethrough`.

To commit many changes quickly, group them in one transaction: the flush is paid once per commit,
not once per statement.
