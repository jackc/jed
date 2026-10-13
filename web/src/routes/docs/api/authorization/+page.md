<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Authorization — jed</title>
	<meta name="description" content="Restrict what a jed session may do with the GRANT/REVOKE privilege envelope — per-table SELECT/INSERT/UPDATE/DELETE, function EXECUTE, and a DDL gate." />
</svelte:head>

# Authorization

jed has no users, roles, or `GRANT` statements in SQL — authorization lives **above** the engine, on
the **session**. A host serving untrusted queries configures a privilege envelope and the engine
enforces it mechanically: any operation the envelope withholds fails with **`42501`** at name
resolution, before it runs.

Pair authorization with the [resource limits](../resource-limits/) for metered work and
covered scalar allocations. Review their memory coverage before exposing arbitrary SQL.

## The model

Two object kinds, each with the PostgreSQL privileges jed has a feature for:

- **Tables** — `SELECT`, `INSERT`, `UPDATE`, `DELETE`.
- **Functions** — `EXECUTE`.

Three layers compose into the effective privilege for an operation on an object:

1. **`default_privileges`** — the table privileges granted to **every** table (the "all tables"
   default). The default is all four; set it to `{SELECT}` for a read-only session.
2. **`grant`** — extra privileges on one object, beyond the default.
3. **`revoke`** — privileges withheld from one object. **Revoke always wins** over a grant and the
   default, so denying is order-independent.

A separate **`allow_ddl`** flag (default on) gates all `CREATE` / `DROP` / `ALTER`.

<CodeTabs topic="authorization" />

## What each statement needs

- **`SELECT`** on every table a statement reads — its `FROM`/`JOIN` tables, subqueries anywhere in
  it, an `INSERT … SELECT` source, and the columns an `UPDATE`/`DELETE` reads in `WHERE` /
  `RETURNING` / an assignment. An `INSERT` reads its target when it names an `ON CONFLICT` target or
  has a `RETURNING` that references a column.
- **`INSERT` / `UPDATE` / `DELETE`** on the write target. A statement that both reads and writes
  needs both: `UPDATE t … WHERE …` needs `UPDATE` *and* `SELECT`; a bare `INSERT INTO t VALUES …`
  needs only `INSERT`; `INSERT … ON CONFLICT (id) DO UPDATE …` needs `INSERT`, `UPDATE`, and
  `SELECT`.
- **Views** are granted like tables. Reading a [view](../../sql/views/) needs `SELECT` on the view
  only, not on the tables its query reads. `CREATE VIEW` needs `allow_ddl` plus `SELECT` on every
  table and view its query reads, so a session cannot create a view over data it could not read.
- **`EXECUTE`** on every named function it calls, wherever the call appears — including an
  aggregate's `FILTER`, a `WITHIN GROUP` or window `OVER (…)` clause, and `ORDER BY` — and on the
  functions in the column defaults an `INSERT` fills or `SET col = DEFAULT` assigns and in the
  target's `CHECK` constraints. Built-in operators (`+`, `=`, …) are never gated — they are pure and
  unavoidable. Revoking `EXECUTE` on `uuidv4()` or `now()` is the easy way to pin a session's
  determinism.
- **`ANALYZE table [(columns)]`** needs `SELECT` on its target plus `allow_ddl`. Collection reads the
  table and transactionally replaces planner facts; this slice has no separate `MAINTAIN` privilege.

## Existence is checked first

A privilege is required only once a name **resolves to a real object**. Selecting from a table that
does not exist is `42P01` (undefined table) even under an empty envelope — authorization gates what
exists, it never reveals what doesn't by turning a different error code.
