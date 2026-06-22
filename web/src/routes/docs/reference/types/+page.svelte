<script lang="ts">
  import spec from 'virtual:jed-spec';

  // Generated from the canonical spec data (spec/types/scalars.toml) at build time — cannot drift
  // from the engine.
  const types = spec.types;

  function list(v: unknown): string {
    return Array.isArray(v) && v.length > 0 ? v.join(', ') : '—';
  }
</script>

<svelte:head>
  <title>Type reference — jed</title>
  <meta name="description" content="jed's scalar type set, generated from the canonical spec." />
</svelte:head>

<h1>Type reference</h1>
<p>
  jed's scalar types. This table is generated at build time from the canonical spec data (<code
    >spec/types/scalars.toml</code
  >), so it always matches the engine. Types marked
  <em>expression only</em> exist as expression/literal types but cannot yet be a column type.
</p>

<div class="not-prose overflow-x-auto rounded-lg border border-slate-200">
  <table class="w-full border-collapse text-sm">
    <thead class="bg-slate-50 text-left">
      <tr>
        <th class="px-3 py-2 font-semibold">Type</th>
        <th class="px-3 py-2 font-semibold">Aliases</th>
        <th class="px-3 py-2 font-semibold">Family</th>
        <th class="px-3 py-2 font-semibold">Storable</th>
        <th class="px-3 py-2 font-semibold">Width</th>
        <th class="px-3 py-2 font-semibold">Range</th>
      </tr>
    </thead>
    <tbody>
      {#each types as t (t.id)}
        <tr class="border-t border-slate-100">
          <td class="px-3 py-1.5 font-mono font-medium text-jed-ink">{t.id}</td>
          <td class="px-3 py-1.5 font-mono text-slate-600">{list(t.aliases)}</td>
          <td class="px-3 py-1.5 text-slate-600">{t.family}</td>
          <td class="px-3 py-1.5 text-slate-600">{t.storable ? 'yes' : 'expression only'}</td>
          <td class="px-3 py-1.5 text-slate-600">{t.bits ? `${t.bits}-bit` : '—'}</td>
          <td class="px-3 py-1.5 font-mono text-slate-600"
            >{t.min !== undefined ? `${t.min} … ${t.max}` : '—'}</td
          >
        </tr>
      {/each}
    </tbody>
  </table>
</div>
