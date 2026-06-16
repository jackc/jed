<script lang="ts">
	import { base } from '$app/paths';
	import { page } from '$app/state';
	import { DOCS_NAV } from '$lib/content/docs-nav.ts';
	import Search from '$lib/components/Search.svelte';

	// Docs chrome: a section sidebar + the prose content area that every docs page (Markdown or
	// Svelte) renders into. `data-pagefind-body` marks the article as the indexed content and
	// `data-pagefind-ignore` keeps the nav out of search results (Pagefind, wired at build).
	let { children } = $props();

	function isActive(href: string): boolean {
		return page.url.pathname === `${base}${href}`;
	}
</script>

<div class="grid grid-cols-1 gap-8 md:grid-cols-[14rem_minmax(0,1fr)]">
	<aside class="md:sticky md:top-4 md:self-start" data-pagefind-ignore>
		<div class="mb-5">
			<Search />
		</div>
		<nav class="space-y-5 text-sm">
			{#each DOCS_NAV as section (section.title)}
				<div>
					<p class="mb-2 text-xs font-semibold tracking-wide text-jed-muted uppercase">
						{section.title}
					</p>
					<ul class="space-y-1 border-l border-slate-200">
						{#each section.links as link (link.href)}
							<li>
								<a
									href="{base}{link.href}"
									class="-ml-px block border-l-2 py-0.5 pl-3 {isActive(link.href)
										? 'border-jed-accent font-medium text-jed-accent'
										: 'border-transparent text-slate-600 hover:border-slate-300 hover:text-jed-ink'}"
								>
									{link.title}
								</a>
							</li>
						{/each}
					</ul>
				</div>
			{/each}
		</nav>
	</aside>

	<article class="prose max-w-none" data-pagefind-body>
		{@render children()}
	</article>
</div>
