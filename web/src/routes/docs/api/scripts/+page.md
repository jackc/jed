<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Running scripts — jed</title>
	<meta name="description" content="Run a multi-statement SQL script (a migration or import) in jed from Rust, Go, or TypeScript." />
</svelte:head>

# Running scripts

To run a whole file of SQL — a migration, a seed, a data import — use **`execute_script`**. It splits
the string into statements, runs each in order, and (when no transaction is open) wraps the lot in
**one implicit transaction**, so the script is all-or-nothing: any statement's error rolls the whole
run back.

It **discards result rows** and returns a small `ScriptSummary` — statements run, total rows
affected, accrued cost. That summary is `O(1)`, so even an import of millions of rows never buffers
results in memory.

<CodeTabs topic="scripts" />

## The splitter is a primitive too

`split_statements` is the library-level building block underneath `execute_script` — a pure,
streaming statement scanner that needs no open database. A `;` inside a string literal, a
dollar-quoted string, or a comment is never treated as a boundary. When you *do* want each
statement's rows (not just a success/fail summary), loop it yourself and run the spans through the
normal `execute` / `query` path — you own the policy (one transaction or autocommit, drain the rows
or drop them).

## Transaction control inside a script

Because `execute_script` owns the implicit transaction boundary, an explicit `BEGIN`, `COMMIT`, or
`ROLLBACK` **inside** the script is rejected (`0A000`). Run on a session that already has a
transaction open and the script simply joins it — no wrapper, no auto-commit, so the caller stays in
control. For a script that manages its own transactions, use the `split_statements` loop instead.
