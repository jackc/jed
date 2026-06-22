<script lang="ts">
  import '../app.css';
  import favicon from '$lib/assets/favicon.svg';
  import { base } from '$app/paths';
  import LangSelector from '$lib/components/LangSelector.svelte';

  let { children } = $props();

  // Top-level nav. The language selector (Phase 3) mounts here too, but is meaningful chiefly in
  // the API docs (it switches host-language example code, not the SQL the live panels run).
  const nav = [
    { href: `${base}/`, label: 'Home' },
    { href: `${base}/docs/`, label: 'Docs' },
    { href: `${base}/tool/`, label: 'Tool' }
  ];
</script>

<svelte:head>
  <link rel="icon" href={favicon} />
</svelte:head>

<div class="flex min-h-screen flex-col">
  <header class="border-b border-slate-200 bg-white/80 backdrop-blur">
    <div class="mx-auto flex max-w-6xl items-center gap-6 px-4 py-3">
      <a href="{base}/" class="text-lg font-bold tracking-tight text-jed-ink">jed</a>
      <nav class="flex gap-4 text-sm text-jed-muted">
        {#each nav as item (item.href)}
          <a class="hover:text-jed-accent" href={item.href}>{item.label}</a>
        {/each}
      </nav>
      <div class="ml-auto flex items-center gap-3">
        <span class="hidden text-xs text-jed-muted sm:inline">example language:</span>
        <LangSelector />
      </div>
    </div>
  </header>

  <main class="mx-auto w-full max-w-6xl flex-1 px-4 py-8">
    {@render children()}
  </main>

  <footer class="border-t border-slate-200 py-6 text-center text-xs text-jed-muted">
    jed — an embeddable, strictly-typed SQL database.
  </footer>
</div>
