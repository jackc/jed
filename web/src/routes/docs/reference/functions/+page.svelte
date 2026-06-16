<script lang="ts">
	import spec from 'virtual:jed-spec';

	// Generated from the canonical function/operator catalog (spec/functions/catalog.toml).
	const operators = spec.operators;
	const aggregates = spec.aggregates;

	function args(v: unknown): string {
		return Array.isArray(v) && v.length > 0 ? v.join(', ') : '—';
	}
</script>

<svelte:head>
	<title>Functions &amp; operators — jed</title>
	<meta
		name="description"
		content="jed's operator and aggregate catalog, generated from the canonical spec."
	/>
</svelte:head>

<h1>Functions &amp; operators</h1>
<p>
	The operator and aggregate catalog, generated at build time from
	<code>spec/functions/catalog.toml</code>. <code>null</code> describes NULL behavior (for example,
	<code>kleene</code> three-valued logic, or <code>strict</code> = NULL in, NULL out).
</p>

<h2>Operators</h2>
<div class="not-prose mb-6 overflow-x-auto rounded-lg border border-slate-200">
	<table class="w-full border-collapse text-sm">
		<thead class="bg-slate-50 text-left">
			<tr>
				<th class="px-3 py-2 font-semibold">Name</th>
				<th class="px-3 py-2 font-semibold">Kind</th>
				<th class="px-3 py-2 font-semibold">Argument types</th>
				<th class="px-3 py-2 font-semibold">Result</th>
				<th class="px-3 py-2 font-semibold">NULL</th>
			</tr>
		</thead>
		<tbody>
			{#each operators as op, i (i)}
				<tr class="border-t border-slate-100">
					<td class="px-3 py-1.5 font-mono font-medium text-jed-ink">{op.name}</td>
					<td class="px-3 py-1.5 text-slate-600">{op.kind}</td>
					<td class="px-3 py-1.5 font-mono text-slate-600">{args(op.arg_families)}</td>
					<td class="px-3 py-1.5 font-mono text-slate-600">{op.result}</td>
					<td class="px-3 py-1.5 text-slate-600">{op.null}</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<h2>Aggregates</h2>
<div class="not-prose overflow-x-auto rounded-lg border border-slate-200">
	<table class="w-full border-collapse text-sm">
		<thead class="bg-slate-50 text-left">
			<tr>
				<th class="px-3 py-2 font-semibold">Name</th>
				<th class="px-3 py-2 font-semibold">Argument types</th>
				<th class="px-3 py-2 font-semibold">Result</th>
			</tr>
		</thead>
		<tbody>
			{#each aggregates as ag, i (i)}
				<tr class="border-t border-slate-100">
					<td class="px-3 py-1.5 font-mono font-medium text-jed-ink">{ag.name}</td>
					<td class="px-3 py-1.5 font-mono text-slate-600">{args(ag.arg_families)}</td>
					<td class="px-3 py-1.5 font-mono text-slate-600">{ag.result}</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>
