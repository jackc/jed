<script lang="ts">
  import spec from 'virtual:jed-spec';

  // Generated from the canonical error registry (spec/errors/registry.toml). Errors are structured
  // data with a stable SQLSTATE — the same codes the live panels and the tool surface.
  const errors = spec.errors;
</script>

<svelte:head>
  <title>Error codes — jed</title>
  <meta
    name="description"
    content="jed's structured error codes (SQLSTATE), from the canonical registry."
  />
</svelte:head>

<h1>Error codes</h1>
<p>
  Every jed error carries a stable SQLSTATE code and symbolic name — machine-legible, never matched
  on prose. This registry is generated at build time from <code>spec/errors/registry.toml</code>.
  These are the codes you'll see in the live examples and the database tool (for example,
  <code>54P01</code> when a query exceeds its cost ceiling).
</p>

<div class="not-prose overflow-x-auto rounded-lg border border-slate-200">
  <table class="w-full border-collapse text-sm">
    <thead class="bg-slate-50 text-left">
      <tr>
        <th class="px-3 py-2 font-semibold">SQLSTATE</th>
        <th class="px-3 py-2 font-semibold">Name</th>
        <th class="px-3 py-2 font-semibold">Class</th>
        <th class="px-3 py-2 font-semibold">Message template</th>
      </tr>
    </thead>
    <tbody>
      {#each errors as e (e.code)}
        <tr class="border-t border-slate-100">
          <td class="px-3 py-1.5 font-mono font-medium text-jed-ink">{e.code}</td>
          <td class="px-3 py-1.5 font-mono text-slate-600">{e.name}</td>
          <td class="px-3 py-1.5 text-slate-600">{e.class}</td>
          <td class="px-3 py-1.5 text-slate-600">{e.template}</td>
        </tr>
      {/each}
    </tbody>
  </table>
</div>
