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

In Go, call `db.Update(ctx, fn)` / `db.View(ctx, fn)` or the same methods on a session.
Both roll back and release their locks and snapshot pins before propagating a panic.
The context controls transaction admission and the check before commit; pass it to
`tx.Exec` / `tx.Query` inside the callback to cancel those statements too. Callbacks
run synchronously and are never forcibly interrupted.

For a manual block, `session.Begin(ctx, true)` starts a writable transaction and
`session.Begin(ctx, false)` starts a read-only one. That context controls beginning
the transaction only; after it returns, use each statement's context and explicitly
call `session.Commit()` or `session.Rollback()`. A deferred `session.Rollback()` is
safe after commit. Cancellation does not disable rollback or session cleanup.

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

With the default `synchronous = on`, a successful file-backed commit makes its persistent changes
durable. Steady-state commits write changed pages and a validation manifest, then issue one durable
flush. File growth and recovery may require additional flushes; a transaction with no persistent
changes needs none. On macOS, jed uses `F_FULLFSYNC`; latency depends on the storage device and host.

With `synchronous = off`, commits become visible before the later batch flush and can be lost after
an OS crash or power failure. `skip_fsync` is a separate, unsafe scratch-data option. See
[durability settings](../opening-a-database/#durability) for the guarantees and flush interval.

Group related writes in one transaction to make them atomic and amortize durable flushing.
