<script lang="ts">
  import { base } from '$app/paths';
  import LiveSql from '$lib/components/LiveSql.svelte';

  // Home (Phase 4) — clean & minimal: the north-star pitch, a LIVE in-memory jed database the
  // visitor can edit and run in their browser, and the key properties. No server is involved.
  const seed = `CREATE TABLE person (
  id    i32 PRIMARY KEY,
  name  text NOT NULL,
  score numeric(6,2)
);
INSERT INTO person VALUES
  (1, 'Ada',   91.5),
  (2, 'Linus', 88),
  (3, 'Grace', 99.25);`;

  const query = `SELECT name, score
FROM person
WHERE score > 90
ORDER BY score DESC;`;

  const features = [
    {
      title: 'Embeddable',
      body: 'A library you link into your program — no server, no daemon. Just your app and a file.'
    },
    {
      title: 'Single file',
      body: 'One database is one file on disk. Copy it, back it up, ship it. Commits are atomic and crash-safe.'
    },
    {
      title: 'Rich type system',
      body: 'Exact decimals, timestamps, UUID, JSON/JSONB, arrays, ranges, and composite types — and a value is never silently reinterpreted at runtime.'
    },
    {
      title: 'PostgreSQL-modeled behavior',
      body: 'Modeled on PostgreSQL — not just the syntax it accepts, but the semantics a query actually observes. The everyday SQL you already write behaves the same.'
    },
    {
      title: 'Safe for untrusted SQL',
      body: 'Safe by design — memory-safe cores, a pure built-in surface with no host access, and a deterministic cost ceiling on every query.'
    },
    {
      title: 'Many native cores',
      body: 'Built from scratch in Rust, Go, and TypeScript in lockstep — byte-identical, no reference implementation.'
    }
  ];
</script>

<svelte:head>
  <title>jed — embeddable, capable, safe for untrusted SQL</title>
  <meta
    name="description"
    content="jed is an embeddable SQL database: one file, no server, safe to run untrusted queries, with types and behavior modeled on PostgreSQL. Try it live in your browser."
  />
</svelte:head>

<section class="py-6 text-center">
  <h1 class="text-4xl font-bold tracking-tight text-jed-ink sm:text-5xl">jed</h1>
  <p class="mx-auto mt-4 max-w-2xl text-xl font-semibold text-jed-ink sm:text-2xl">
    Embeddable. Capable. <span class="text-jed-accent">Safe for untrusted SQL.</span>
  </p>
  <p class="mx-auto mt-2 max-w-2xl text-base text-slate-600">
    One file, no server — types and behavior modeled on PostgreSQL.
  </p>
  <div class="mt-6 flex justify-center gap-3">
    <a
      href="{base}/docs/"
      class="rounded-md bg-jed-accent px-4 py-2 text-sm font-semibold text-white hover:bg-blue-700"
      >Read the docs</a
    >
    <a
      href="{base}/tool/"
      class="rounded-md border border-slate-300 px-4 py-2 text-sm font-semibold text-jed-ink hover:bg-slate-50"
      >Open the database tool</a
    >
  </div>
  <p class="mx-auto mt-5 max-w-2xl text-sm text-jed-muted">
    <span class="font-semibold text-jed-ink">0.x preview.</span> Heavily tested, but not yet proven
    by real-world use — and the on-disk format may still change.
    <a class="text-jed-accent hover:underline" href="{base}/docs/status/"
      >Read the preview status →</a
    >
  </p>
</section>

<section class="mt-4">
  <div class="mb-2 flex items-baseline justify-between">
    <h2 class="text-sm font-semibold tracking-wide text-jed-muted uppercase">
      A live database, in your browser
    </h2>
    <span class="text-xs text-jed-muted">no server — the engine is running on this page</span>
  </div>
  <LiveSql {seed} {query} title="Try it" rows={8} />
</section>

<section class="mt-10">
  <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
    {#each features as f (f.title)}
      <div class="rounded-lg border border-slate-200 bg-white p-4">
        <h3 class="font-semibold text-jed-ink">{f.title}</h3>
        <p class="mt-1 text-sm text-slate-600">{f.body}</p>
      </div>
    {/each}
  </div>
</section>

<section class="mt-10">
  <h2 class="mb-2 text-sm font-semibold tracking-wide text-jed-muted uppercase">Built for</h2>
  <div class="grid gap-4 sm:grid-cols-3">
    <div class="rounded-lg border border-slate-200 bg-white p-4">
      <h3 class="font-semibold text-jed-ink">Running untrusted SQL</h3>
      <p class="mt-1 text-sm text-slate-600">
        Let your users write and run their own queries. An adversarial query still cannot corrupt
        memory, reach the host, or exhaust resources.
      </p>
    </div>
    <div class="rounded-lg border border-slate-200 bg-white p-4">
      <h3 class="font-semibold text-jed-ink">Local dev &amp; test</h3>
      <p class="mt-1 text-sm text-slate-600">
        When production is PostgreSQL, run your test suite and local development against an embedded
        database — no server to provision, behavior you can rely on.
      </p>
    </div>
    <div class="rounded-lg border border-slate-200 bg-white p-4">
      <h3 class="font-semibold text-jed-ink">An embedded PostgreSQL companion</h3>
      <p class="mt-1 text-sm text-slate-600">
        Embed jed at small scale, and scale up to PostgreSQL with the same SQL. One codebase, two
        footprints.
      </p>
    </div>
  </div>
</section>

<section class="mt-10 rounded-lg border border-slate-200 bg-slate-50 p-5">
  <h2 class="font-semibold text-jed-ink">How it works here</h2>
  <p class="mt-1 text-sm text-slate-600">
    This whole site is static — no backend. The jed engine is compiled to run in your browser: a
    native TypeScript core executes in a Web Worker, and the
    <a class="text-jed-accent hover:underline" href="{base}/tool/">database tool</a> persists real single-file
    databases to your browser’s origin-private file system (OPFS). The same engine ships as native Rust,
    Go, and TypeScript cores.
  </p>
</section>
