<script lang="ts">
	import { onMount } from 'svelte';
	import { base } from '$app/paths';

	// Docs search, built on Pagefind's JS API (CLAUDE.md §6). Pagefind indexes the BUILT static HTML
	// after `vite build` (see the build script) and ships a tiny runtime that loads index shards on
	// demand — zero server, perfect for a static site. We use the JS API directly (not the prebuilt
	// UI) for full Tailwind styling and base-path correctness. In dev there is no index, so the box
	// degrades gracefully.

	type Result = { url: string; title: string; excerpt: string };

	let query = $state('');
	let results = $state<Result[]>([]);
	let open = $state(false);
	let unavailable = $state(false);
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	let pagefind: any = null;

	onMount(() => {
		(async () => {
			try {
				// Runtime URL (only exists in the built site); @vite-ignore so Vite doesn't try to bundle it.
				pagefind = await import(/* @vite-ignore */ `${base}/pagefind/pagefind.js`);
				await pagefind.init();
			} catch {
				unavailable = true;
			}
		})();
	});

	async function run(): Promise<void> {
		if (pagefind === null || query.trim() === '') {
			results = [];
			open = false;
			return;
		}
		const search = await pagefind.search(query);
		// eslint-disable-next-line @typescript-eslint/no-explicit-any
		const data = await Promise.all(search.results.slice(0, 8).map((r: any) => r.data()));
		results = data.map(
			// eslint-disable-next-line @typescript-eslint/no-explicit-any
			(d: any): Result => ({ url: d.url, title: d.meta?.title ?? d.url, excerpt: d.excerpt })
		);
		open = results.length > 0;
	}
</script>

<div class="relative" data-pagefind-ignore>
	<input
		type="search"
		bind:value={query}
		oninput={run}
		onfocus={() => (open = results.length > 0)}
		disabled={unavailable}
		placeholder={unavailable ? 'Search (built site only)' : 'Search docs…'}
		data-testid="search-input"
		class="w-full rounded-md border border-slate-300 bg-white px-2.5 py-1.5 text-sm focus:border-jed-accent focus:outline-none disabled:bg-slate-50 disabled:text-jed-muted"
	/>
	{#if open}
		<ul
			class="absolute z-20 mt-1 max-h-96 w-full overflow-auto rounded-md border border-slate-200 bg-white shadow-lg"
			data-testid="search-results"
		>
			{#each results as r (r.url)}
				<li class="border-b border-slate-100 last:border-0">
					<a href="{base}{r.url}" class="block px-3 py-2 hover:bg-slate-50" onclick={() => (open = false)}>
						<span class="text-sm font-medium text-jed-ink">{r.title}</span>
						<!-- eslint-disable-next-line svelte/no-at-html-tags - Pagefind excerpt from our own indexed content -->
						<span class="mt-0.5 block text-xs text-jed-muted">{@html r.excerpt}</span>
					</a>
				</li>
			{/each}
		</ul>
	{/if}
</div>
