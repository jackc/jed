<script lang="ts">
	import type { RunResult } from '$lib/jed/protocol.ts';
	import { JedError } from '$lib/jed/session.ts';

	// ResultGrid renders the outcome of running SQL: the last query's rows (with column names +
	// types + a cost footer), a non-query statement's command tag, or a structured SQLSTATE error.
	// `not-prose` keeps Tailwind typography from restyling the grid when it sits inside Markdown.
	let {
		results = [],
		error = null
	}: { results?: RunResult[]; error?: JedError | null } = $props();

	// The query to display in the grid is the LAST query in the batch (CREATE; INSERT; SELECT -> the
	// SELECT). Non-query statements are summarized in the log line.
	const lastQuery = $derived(
		[...results].reverse().find((r): r is Extract<RunResult, { kind: 'query' }> => r.kind === 'query')
	);

	// A numeric column (int/decimal) is right-aligned, mirroring the CLI's aligned format.
	function isNumeric(type: string): boolean {
		return /^(int16|int32|int64|decimal|numeric|float32|float64)/.test(type);
	}

	const log = $derived(
		results.map((r) =>
			r.kind === 'query'
				? `(${r.rowCount} row${r.rowCount === 1 ? '' : 's'}, cost ${r.cost})`
				: `${r.tag} (cost ${r.cost})`
		)
	);
</script>

<div class="not-prose text-sm" data-testid="result-grid">
	{#if error}
		<div
			class="rounded-md border border-red-300 bg-red-50 px-3 py-2 font-mono text-red-800"
			data-testid="result-error"
		>
			<span class="font-semibold" data-testid="error-code">{error.code}</span>: {error.message}
		</div>
	{:else if results.length === 0}
		<p class="text-jed-muted">No results yet. Run a query to see output.</p>
	{:else}
		{#if lastQuery}
			<div class="overflow-x-auto rounded-md border border-slate-200">
				<table class="w-full border-collapse font-mono text-xs">
					<thead class="bg-slate-50">
						<tr>
							{#each lastQuery.columnNames as name, i (i)}
								<th class="border-b border-slate-200 px-3 py-1.5 text-left font-semibold">
									{name}
									<span class="ml-1 font-normal text-jed-muted">{lastQuery.columnTypes[i]}</span>
								</th>
							{/each}
						</tr>
					</thead>
					<tbody data-testid="result-rows">
						{#each lastQuery.rows as row, ri (ri)}
							<tr class="odd:bg-white even:bg-slate-50/50">
								{#each row as cell, ci (ci)}
									<td
										class="border-b border-slate-100 px-3 py-1 {isNumeric(
											lastQuery.columnTypes[ci] ?? ''
										)
											? 'text-right'
											: 'text-left'} {cell === 'NULL' ? 'text-slate-400 italic' : ''}"
									>
										{cell}
									</td>
								{/each}
							</tr>
						{/each}
						{#if lastQuery.rows.length === 0}
							<tr><td class="px-3 py-2 text-jed-muted" colspan={lastQuery.columnNames.length}
									>(0 rows)</td
								></tr>
						{/if}
					</tbody>
				</table>
			</div>
		{/if}
		<ul class="mt-2 space-y-0.5 font-mono text-xs text-jed-muted" data-testid="result-log">
			{#each log as line, i (i)}
				<li>{line}</li>
			{/each}
		</ul>
	{/if}
</div>
