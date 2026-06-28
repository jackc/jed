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

In **Rust and Go**, the `update` helper runs a read-write transaction that commits on success and
rolls back if your code signals an error — the safest default; there's a read-only `view` helper too.
The explicit `begin` / `commit` / `rollback` form is available in every language for finer control,
and is how **TypeScript** drives a block (it has no closure helper). All of these run on the
`Database` handle directly, or on any session you've minted from it.

<CodeTabs topic="transactions" />

## Isolation

Readers see the last committed state and run without blocking against an in-flight writer; the only
exclusive moment is the commit itself. This is **not** MVCC — there is exactly one committed version
plus the current writer's pending changes. It keeps the model simple and the read path nearly
lock-free.
