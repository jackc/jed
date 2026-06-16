<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Opening a database — jed</title>
	<meta name="description" content="Open or create a single-file jed database from Rust, Go, or TypeScript." />
</svelte:head>

# Opening a database

A jed database is a single file on disk. Open or create one, run SQL against it, and commit when
you're done. Pass a path for a durable file, or open a transient **in-memory** database for tests
and scratch work.

Use the **language selector** in the top bar to switch this example between Rust, Go, and
TypeScript.

<CodeTabs topic="open-database" />

## Durability

Writes accumulate until you **commit**. Closing a database discards uncommitted changes — commit is
always explicit. An in-memory database's commit is a no-op (there is no file to flush). Commits are
durable: the new state lands on disk before the call returns.

## In-memory databases

Every example on the **SQL** pages of these docs runs against an in-memory database, right in your
browser — the same engine, no file. Create one with `Database::new()` (Rust), `jed.NewDatabase()`
(Go), or `new Database()` (TypeScript).
