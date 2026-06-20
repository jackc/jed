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

This is the concrete form of jed's "untrusted SQL is safe to run" guarantee — pair it with the cost
ceiling (`max_cost`) and you can hand an adversary a query surface.

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

- **`SELECT`** on every table a statement reads — its `FROM`/`JOIN` tables, subqueries, an
  `INSERT … SELECT` source, and the columns an `UPDATE`/`DELETE` reads in `WHERE` / `RETURNING` /
  an assignment.
- **`INSERT` / `UPDATE` / `DELETE`** on the write target. A statement that both reads and writes
  needs both: `UPDATE t … WHERE …` needs `UPDATE` *and* `SELECT`; a bare `INSERT INTO t VALUES …`
  needs only `INSERT`.
- **`EXECUTE`** on every named function it calls. Built-in operators (`+`, `=`, …) are never gated —
  they are pure and unavoidable. Revoking `EXECUTE` on `uuidv4()` or `now()` is the easy way to pin a
  session's determinism.

## Existence is checked first

A privilege is required only once a name **resolves to a real object**. Selecting from a table that
does not exist is `42P01` (undefined table) even under an empty envelope — authorization gates what
exists, it never reveals what doesn't by turning a different error code.
