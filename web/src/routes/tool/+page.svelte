<script lang="ts">
  import { onMount } from 'svelte';
  import ResultGrid from '$lib/components/ResultGrid.svelte';
  import SqlEditor from '$lib/components/SqlEditor.svelte';
  import { formatQuery, type OutputFormat } from '$lib/jed/format.ts';
  import type { OpfsFileInfo, RunResult, SchemaResult } from '$lib/jed/protocol.ts';
  import {
    createOpfsDb,
    deleteOpfsFile,
    exportOpfsFile,
    importOpfsFile,
    JedError,
    listOpfsFiles,
    openMemory,
    openOpfsDb,
    opfsSupported,
    type JedDb
  } from '$lib/jed/session.ts';

  // The full database tool (CLI/TUI parity): a CodeMirror SQL editor, a results grid with output
  // formats, a schema sidebar, transaction-state + cost indicators, and an OPFS file manager for
  // real local single-file databases. Entirely client-side — the engine runs in the shared Web
  // Worker (session.ts). Always has a working in-memory database; OPFS adds persistence when the
  // browser supports it (graceful degradation otherwise).

  const DEFAULT_MAX_COST = 50_000_000n;
  const DEFAULT_SQL = `-- A scratch in-memory database. Create a table, or open / import a .jed file on the left.
CREATE TABLE note (
  id   i32 PRIMARY KEY,
  body text NOT NULL
);
INSERT INTO note VALUES (1, 'hello'), (2, 'world');
SELECT * FROM note ORDER BY id;`;

  let db = $state<JedDb | null>(null);
  let dbLabel = $state('in-memory');
  let isOpfs = $state(false);
  let sql = $state(DEFAULT_SQL);
  let results = $state<RunResult[]>([]);
  let error = $state<JedError | null>(null);
  let running = $state(false);
  let schema = $state<SchemaResult | null>(null);
  let format = $state<OutputFormat>('table');
  let maxCost = $state('50000000');

  // OPFS file manager state.
  let opfsOk = $state(false);
  let files = $state<OpfsFileInfo[]>([]);
  let newName = $state('');
  let busy = $state(false);
  let fileMsg = $state<string | null>(null);

  const lastQuery = $derived(
    [...results]
      .reverse()
      .find((r): r is Extract<RunResult, { kind: 'query' }> => r.kind === 'query')
  );
  const formatted = $derived(lastQuery && format !== 'table' ? formatQuery(lastQuery, format) : '');
  const totalCost = $derived(results.reduce((acc, r) => acc + BigInt(r.cost), 0n).toString());
  const inTx = $derived(schema?.inTransaction ?? false);

  async function refreshSchema(): Promise<void> {
    if (db) schema = await db.schema();
  }
  async function refreshFiles(): Promise<void> {
    if (opfsOk) files = await listOpfsFiles();
  }

  async function run(): Promise<void> {
    if (!db || running) return;
    running = true;
    error = null;
    try {
      results = await db.run(sql);
    } catch (e) {
      results = [];
      error = e instanceof JedError ? e : new JedError('XX000', String(e));
    } finally {
      running = false;
      await refreshSchema();
    }
  }

  async function commit(): Promise<void> {
    if (db) {
      await db.commit();
      await refreshSchema();
    }
  }
  async function rollback(): Promise<void> {
    if (db) {
      await db.rollback();
      await refreshSchema();
    }
  }

  async function applyMaxCost(): Promise<void> {
    try {
      if (db) await db.setMaxCost(BigInt(maxCost || '0'));
    } catch {
      /* ignore a malformed value */
    }
  }

  async function closeCurrent(): Promise<void> {
    if (db) {
      await db.close();
      db = null;
      schema = null;
    }
  }

  async function useMemory(): Promise<void> {
    await closeCurrent();
    db = await openMemory({ maxCost: BigInt(maxCost || '0') });
    dbLabel = 'in-memory';
    isOpfs = false;
    results = [];
    error = null;
    await refreshSchema();
  }

  async function createFile(): Promise<void> {
    const name = withExt(newName.trim());
    if (!name) return;
    busy = true;
    fileMsg = null;
    try {
      await closeCurrent();
      db = await createOpfsDb(name, { maxCost: BigInt(maxCost || '0') });
      dbLabel = name;
      isOpfs = true;
      results = [];
      error = null;
      newName = '';
      await refreshSchema();
      await refreshFiles();
    } catch (e) {
      fileMsg = errText(e);
      await useMemory();
    } finally {
      busy = false;
    }
  }

  async function openFile(name: string): Promise<void> {
    busy = true;
    fileMsg = null;
    try {
      await closeCurrent();
      db = await openOpfsDb(name, { maxCost: BigInt(maxCost || '0') });
      dbLabel = name;
      isOpfs = true;
      results = [];
      error = null;
      await refreshSchema();
    } catch (e) {
      fileMsg = errText(e);
      await useMemory();
    } finally {
      busy = false;
    }
  }

  async function dropFile(name: string): Promise<void> {
    busy = true;
    fileMsg = null;
    try {
      if (isOpfs && dbLabel === name) await useMemory(); // release the handle first
      await deleteOpfsFile(name);
      await refreshFiles();
    } catch (e) {
      fileMsg = errText(e);
    } finally {
      busy = false;
    }
  }

  async function exportFile(name: string): Promise<void> {
    busy = true;
    fileMsg = null;
    try {
      // Export reads the file's bytes — the exclusive sync handle must be released first, so close
      // the database if it's the one being exported (the single-writer rule, CLAUDE.md §3).
      if (isOpfs && dbLabel === name) await useMemory();
      const bytes = await exportOpfsFile(name);
      download(name, bytes);
    } catch (e) {
      fileMsg = errText(e);
    } finally {
      busy = false;
    }
  }

  async function importFile(ev: Event): Promise<void> {
    const input = ev.target as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    busy = true;
    fileMsg = null;
    try {
      const name = withExt(file.name);
      const bytes = new Uint8Array(await file.arrayBuffer());
      await importOpfsFile(name, bytes, true);
      await refreshFiles();
      fileMsg = `Imported ${name} — open it from the list.`;
    } catch (e) {
      fileMsg = errText(e);
    } finally {
      busy = false;
      input.value = '';
    }
  }

  function withExt(n: string): string {
    return n ? (n.endsWith('.jed') ? n : `${n}.jed`) : '';
  }
  function errText(e: unknown): string {
    return e instanceof JedError ? `${e.code}: ${e.message}` : String(e);
  }
  function download(name: string, bytes: Uint8Array): void {
    // Copy into a fresh ArrayBuffer-backed view so the type is a plain BlobPart (TS DOM lib).
    const part = new Uint8Array(bytes);
    const url = URL.createObjectURL(new Blob([part], { type: 'application/octet-stream' }));
    const a = document.createElement('a');
    a.href = url;
    a.download = name;
    a.click();
    URL.revokeObjectURL(url);
  }
  async function copyFormatted(): Promise<void> {
    try {
      await navigator.clipboard.writeText(formatted);
    } catch {
      /* clipboard may be blocked; ignore */
    }
  }

  onMount(() => {
    let disposed = false;
    (async () => {
      opfsOk = await opfsSupported();
      db = await openMemory({ maxCost: DEFAULT_MAX_COST });
      if (disposed) {
        await db.close();
        return;
      }
      await refreshSchema();
      if (opfsOk) await refreshFiles();
      await run();
    })();
    return () => {
      disposed = true;
      db?.close();
    };
  });
</script>

<svelte:head>
  <title>Database tool — jed</title>
  <meta
    name="description"
    content="A full jed database tool in your browser: SQL editor, schema, and local databases via OPFS."
  />
</svelte:head>

<div class="mb-3">
  <h1 class="text-xl font-bold text-jed-ink">Database tool</h1>
  <p class="text-sm text-jed-muted">
    A real jed database, in your browser. The scratch database is in memory; create, open, import,
    or export single-file <code>.jed</code> databases stored in your browser's private file system (OPFS).
  </p>
</div>

<div class="grid grid-cols-1 gap-4 lg:grid-cols-[16rem_minmax(0,1fr)]">
  <!-- LEFT: schema + OPFS file manager -->
  <aside class="space-y-4">
    <section class="rounded-lg border border-slate-200 bg-white" data-testid="schema-sidebar">
      <h2
        class="border-b border-slate-200 px-3 py-2 text-xs font-semibold tracking-wide text-jed-muted uppercase"
      >
        Schema
      </h2>
      <div class="max-h-72 overflow-auto p-3 text-sm">
        {#if schema && schema.tables.length > 0}
          {#each schema.tables as t (t.name)}
            <details class="mb-1" open>
              <summary class="cursor-pointer font-mono font-medium text-jed-ink">{t.name}</summary>
              <ul class="mt-1 ml-3 space-y-0.5">
                {#each t.columns as c (c.name)}
                  <li class="font-mono text-xs text-slate-600">
                    {c.name}
                    <span class="text-jed-muted">{c.type}</span>
                    {#if c.primaryKey}<span class="text-amber-600">PK</span>{/if}
                    {#if c.notNull && !c.primaryKey}<span class="text-slate-400">NN</span>{/if}
                  </li>
                {/each}
                {#each t.indexes as ix (ix.name)}
                  <li class="font-mono text-xs text-slate-400">
                    index {ix.name}{ix.unique ? ' (unique)' : ''}
                  </li>
                {/each}
              </ul>
            </details>
          {/each}
        {:else}
          <p class="text-xs text-jed-muted">No tables yet.</p>
        {/if}
      </div>
    </section>

    <section class="rounded-lg border border-slate-200 bg-white" data-testid="opfs-panel">
      <h2
        class="border-b border-slate-200 px-3 py-2 text-xs font-semibold tracking-wide text-jed-muted uppercase"
      >
        Local databases (OPFS)
      </h2>
      <div class="p-3 text-sm">
        {#if !opfsOk}
          <p class="text-xs text-jed-muted" data-testid="opfs-unsupported">
            This browser can't store local databases (OPFS sync access handles unavailable). The
            in-memory editor still works.
          </p>
        {:else}
          <div class="flex gap-1">
            <input
              bind:value={newName}
              placeholder="name"
              data-testid="new-db-name"
              class="min-w-0 flex-1 rounded-md border border-slate-300 px-2 py-1 text-xs focus:border-jed-accent focus:outline-none"
            />
            <button
              onclick={createFile}
              disabled={busy || newName.trim() === ''}
              data-testid="create-db"
              class="rounded-md bg-jed-accent px-2 py-1 text-xs font-medium text-white hover:bg-blue-700 disabled:opacity-50"
            >
              Create
            </button>
          </div>

          <ul class="mt-2 space-y-1" data-testid="file-list">
            {#each files as f (f.name)}
              <li class="flex items-center gap-1 text-xs">
                <button
                  onclick={() => openFile(f.name)}
                  disabled={busy}
                  class="min-w-0 flex-1 truncate text-left font-mono text-jed-accent hover:underline {dbLabel ===
                  f.name
                    ? 'font-semibold'
                    : ''}"
                  title="open {f.name}"
                >
                  {f.name}
                </button>
                <button
                  onclick={() => exportFile(f.name)}
                  disabled={busy}
                  class="text-slate-500 hover:text-jed-ink"
                  title="export">↓</button
                >
                <button
                  onclick={() => dropFile(f.name)}
                  disabled={busy}
                  class="text-slate-400 hover:text-red-600"
                  title="delete">✕</button
                >
              </li>
            {:else}
              <li class="text-xs text-jed-muted">No databases yet.</li>
            {/each}
          </ul>

          <label class="mt-2 block cursor-pointer text-xs text-jed-accent hover:underline">
            Import a .jed file…
            <input
              type="file"
              accept=".jed"
              class="hidden"
              onchange={importFile}
              data-testid="import-input"
            />
          </label>
        {/if}
        {#if fileMsg}
          <p class="mt-2 text-xs text-jed-muted" data-testid="file-msg">{fileMsg}</p>
        {/if}
      </div>
    </section>
  </aside>

  <!-- RIGHT: toolbar + editor + actions + results -->
  <div class="space-y-3">
    <div class="flex flex-wrap items-center gap-3 text-sm">
      <span class="rounded-md bg-slate-100 px-2 py-1 font-mono text-xs" data-testid="db-label">
        {dbLabel}{isOpfs ? '' : ' (memory)'}
      </span>
      {#if isOpfs}
        <button onclick={useMemory} class="text-xs text-jed-accent hover:underline"
          >switch to in-memory</button
        >
      {/if}
      <label class="ml-auto flex items-center gap-1 text-xs text-jed-muted">
        format
        <select
          bind:value={format}
          data-testid="format-select"
          class="rounded-md border border-slate-300 px-1.5 py-1 text-xs focus:border-jed-accent focus:outline-none"
        >
          <option value="table">table</option>
          <option value="csv">csv</option>
          <option value="json">json</option>
          <option value="markdown">markdown</option>
        </select>
      </label>
      <label class="flex items-center gap-1 text-xs text-jed-muted">
        max&nbsp;cost
        <input
          bind:value={maxCost}
          onchange={applyMaxCost}
          inputmode="numeric"
          data-testid="max-cost"
          class="w-24 rounded-md border border-slate-300 px-1.5 py-1 text-xs focus:border-jed-accent focus:outline-none"
        />
      </label>
    </div>

    <SqlEditor bind:value={sql} onrun={run} />

    <div class="flex flex-wrap items-center gap-2">
      <button
        onclick={run}
        disabled={!db || running}
        data-testid="run-button"
        class="rounded-md bg-jed-accent px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-700 disabled:opacity-50"
      >
        {running ? 'Running…' : 'Run ▶'}
      </button>
      <button
        onclick={commit}
        disabled={!db}
        data-testid="commit-button"
        class="rounded-md border border-slate-300 px-3 py-1.5 text-sm hover:bg-slate-50"
        >Commit</button
      >
      <button
        onclick={rollback}
        disabled={!db}
        data-testid="rollback-button"
        class="rounded-md border border-slate-300 px-3 py-1.5 text-sm hover:bg-slate-50"
        >Rollback</button
      >
      <span class="ml-auto flex items-center gap-3 text-xs text-jed-muted">
        <span data-testid="tx-state">{inTx ? 'in transaction' : 'autocommit'}</span>
        <span data-testid="total-cost">cost {totalCost}</span>
      </span>
    </div>

    {#if format === 'table'}
      <ResultGrid {results} {error} />
    {:else if error}
      <ResultGrid {results} {error} />
    {:else if formatted}
      <div class="not-prose">
        <div class="mb-1 flex justify-end">
          <button onclick={copyFormatted} class="text-xs text-jed-accent hover:underline"
            >copy</button
          >
        </div>
        <pre
          data-testid="formatted-output"
          class="overflow-auto rounded-md border border-slate-200 bg-slate-50 p-3 font-mono text-xs">{formatted}</pre>
      </div>
    {:else}
      <p class="text-sm text-jed-muted">Run a query to see {format} output.</p>
    {/if}
  </div>
</div>
