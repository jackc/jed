<script lang="ts">
  import spec from 'virtual:jed-spec';

  // Generated from the canonical function/operator catalog (spec/functions/catalog.toml).
  const operators = spec.operators;
  const aggregates = spec.aggregates;
  const setReturning = spec.setReturning;

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
  The operator, aggregate, and set-returning catalog, generated at build time from
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
<div class="not-prose mb-6 overflow-x-auto rounded-lg border border-slate-200">
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

<h2>Set-returning functions</h2>
<p>
  Called in <code>FROM</code> position as a computed row source — they <em>expand</em> their
  arguments into a set of rows. <code>generate_series</code> yields an integer series;
  <code>unnest</code> (polymorphic over <code>anyarray</code>) yields one row per array element. The
  produced relation has one column named after the function (or its alias).
</p>
<div class="not-prose overflow-x-auto rounded-lg border border-slate-200">
  <table class="w-full border-collapse text-sm">
    <thead class="bg-slate-50 text-left">
      <tr>
        <th class="px-3 py-2 font-semibold">Name</th>
        <th class="px-3 py-2 font-semibold">Argument types</th>
        <th class="px-3 py-2 font-semibold">Result column</th>
        <th class="px-3 py-2 font-semibold">NULL</th>
      </tr>
    </thead>
    <tbody>
      {#each setReturning as srf, i (i)}
        <tr class="border-t border-slate-100">
          <td class="px-3 py-1.5 font-mono font-medium text-jed-ink">{srf.name}</td>
          <td class="px-3 py-1.5 font-mono text-slate-600">{args(srf.arg_families)}</td>
          <td class="px-3 py-1.5 font-mono text-slate-600">{srf.result}</td>
          <td class="px-3 py-1.5 text-slate-600">{srf.null}</td>
        </tr>
      {/each}
    </tbody>
  </table>
</div>
