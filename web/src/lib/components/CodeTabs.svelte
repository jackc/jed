<script lang="ts">
	import examples from 'virtual:jed-examples';
	import { LANGUAGES } from '$lib/content/languages.ts';
	import { lang } from '$lib/stores/lang.ts';

	// CodeTabs shows one example (`topic`) in the currently-selected host language. ALL language
	// variants are rendered into the DOM as build-time Shiki-highlighted HTML (no highlighter JS
	// ships); the selector reveals one via the `hidden` attribute (so switching is instant and there
	// is no hydration mismatch). A language with no variant for this topic shows a graceful fallback —
	// which is how a future core/wrapper appears before its example is written.

	let { topic }: { topic: string } = $props();

	const variants = $derived<Record<string, string>>(examples[topic] ?? {});
	const missing = $derived(variants[$lang] === undefined);
	const currentLabel = $derived(LANGUAGES.find((l) => l.id === $lang)?.label ?? $lang);
</script>

<div class="not-prose my-4" data-testid="code-tabs" data-topic={topic}>
	<div class="overflow-hidden rounded-lg border border-slate-200">
		{#each LANGUAGES as l (l.id)}
			{#if variants[l.id]}
				<div hidden={$lang !== l.id} data-testid="code-{l.id}" data-lang={l.id}>
					<!-- eslint-disable-next-line svelte/no-at-html-tags - build-time Shiki output, not user input -->
					{@html variants[l.id]}
				</div>
			{/if}
		{/each}
		{#if missing}
			<p class="px-4 py-3 text-sm text-jed-muted" data-testid="code-missing">
				No {currentLabel} example for this topic yet.
			</p>
		{/if}
	</div>
</div>
