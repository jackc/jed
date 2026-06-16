<script lang="ts">
	import { LANGUAGES } from '$lib/content/languages.ts';
	import { lang } from '$lib/stores/lang.ts';

	// The host-language selector. Lives in the global nav but is meaningful chiefly in the API docs:
	// it switches every <CodeTabs> (host/embedding example code) site-wide. It does NOT affect the SQL
	// that live panels run (that's identical across languages) — the two axes are orthogonal.
</script>

<div
	class="inline-flex overflow-hidden rounded-md border border-slate-300"
	role="group"
	aria-label="Example language"
	data-testid="lang-selector"
>
	{#each LANGUAGES as l (l.id)}
		<button
			type="button"
			onclick={() => lang.set(l.id)}
			aria-pressed={$lang === l.id}
			data-testid="lang-{l.id}"
			class="px-2.5 py-1 text-xs font-medium transition-colors {$lang === l.id
				? 'bg-jed-accent text-white'
				: 'bg-white text-jed-ink hover:bg-slate-50'}"
		>
			{l.label}
		</button>
	{/each}
</div>
