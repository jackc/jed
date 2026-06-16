<script lang="ts">
	import { onMount } from 'svelte';
	import { openMemory, JedError, type JedDb } from '$lib/jed/session.ts';
	import type { RunResult } from '$lib/jed/protocol.ts';
	import ResultGrid from './ResultGrid.svelte';

	// LiveSql is an editable, runnable in-memory jed database embedded in the page (the home hero and
	// every SQL docs example). It opens a fresh in-memory database on mount, optionally runs a `seed`
	// (setup SQL the reader doesn't edit), then lets the reader edit + run `query` against it — real
	// jed, in the browser. Reset re-seeds. State persists across runs within the widget until reset.
	//
	// The engine runs in the shared Web Worker (session.ts), so a runaway query trips the default cost
	// ceiling (54P01) instead of hanging the page, and the main thread stays responsive.

	let {
		seed = '',
		query = '',
		autorun = true,
		rows = 6,
		title = ''
	}: { seed?: string; query?: string; autorun?: boolean; rows?: number; title?: string } = $props();

	let db: JedDb | null = null;
	// `sql` is an independent editable copy seeded from the `query` prop's initial value (intentional).
	// svelte-ignore state_referenced_locally
	let sql = $state(query);
	let results = $state<RunResult[]>([]);
	let error = $state<JedError | null>(null);
	let running = $state(false);
	let ready = $state(false);

	async function reseed(): Promise<void> {
		if (db === null) return;
		await db.reset();
		if (seed.trim().length > 0) await db.run(seed);
	}

	async function run(): Promise<void> {
		if (db === null || running) return;
		running = true;
		error = null;
		try {
			results = await db.run(sql);
		} catch (e) {
			results = [];
			error = e instanceof JedError ? e : new JedError('XX000', String(e));
		} finally {
			running = false;
		}
	}

	async function reset(): Promise<void> {
		sql = query;
		results = [];
		error = null;
		await reseed();
		if (autorun) await run();
	}

	onMount(() => {
		let disposed = false;
		(async () => {
			db = await openMemory();
			if (disposed) {
				await db.close();
				return;
			}
			if (seed.trim().length > 0) await db.run(seed);
			ready = true;
			if (autorun && sql.trim().length > 0) await run();
		})();
		return () => {
			disposed = true;
			db?.close();
		};
	});

	function onKeydown(e: KeyboardEvent): void {
		// Ctrl/Cmd+Enter runs — the familiar SQL-tool shortcut.
		if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
			e.preventDefault();
			run();
		}
	}
</script>

<div class="not-prose rounded-lg border border-slate-200 bg-white shadow-sm" data-testid="live-sql">
	{#if title}
		<div class="border-b border-slate-200 px-3 py-2 text-sm font-semibold text-jed-ink">{title}</div>
	{/if}
	<div class="p-3">
		<textarea
			bind:value={sql}
			onkeydown={onKeydown}
			spellcheck="false"
			{rows}
			data-testid="sql-input"
			class="w-full resize-y rounded-md border border-slate-300 bg-slate-50 p-2 font-mono text-xs text-jed-ink focus:border-jed-accent focus:outline-none"
		></textarea>
		<div class="mt-2 flex items-center gap-2">
			<button
				onclick={run}
				disabled={!ready || running}
				data-testid="run-button"
				class="rounded-md bg-jed-accent px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-700 disabled:opacity-50"
			>
				{running ? 'Running…' : 'Run'}
			</button>
			<button
				onclick={reset}
				disabled={!ready || running}
				data-testid="reset-button"
				class="rounded-md border border-slate-300 px-3 py-1.5 text-sm font-medium text-jed-ink hover:bg-slate-50 disabled:opacity-50"
			>
				Reset
			</button>
			<span class="ml-auto text-xs text-jed-muted">Ctrl/⌘ + Enter to run · in-memory</span>
		</div>
		<div class="mt-3">
			<ResultGrid {results} {error} />
		</div>
	</div>
</div>
