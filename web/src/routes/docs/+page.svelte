<script lang="ts">
  import { base } from '$app/paths';

  // Docs index / introduction. Sets up the two-axis split the user noted: the embedding API
  // (per-language, selector-driven) vs. the SQL (language-neutral, live-runnable).
</script>

<svelte:head>
  <title>Documentation — jed</title>
  <meta
    name="description"
    content="jed documentation: the embedding API, the SQL, and a generated reference."
  />
</svelte:head>

<h1>Documentation</h1>
<p>
  jed's documentation comes in two parts, because there are two different questions: <em
    >how do I call jed from my program?</em
  >
  and <em>what does the SQL do?</em>
</p>

<h2>Embedding API</h2>
<p>
  How to drive jed from a host program — open a database, run statements, read rows, manage
  transactions. The same operations look a little different in each language, so these pages show
  the code in <strong>Rust, Go, and TypeScript</strong>. Pick your language with the selector in the
  top bar; it switches every example site-wide.
</p>
<p><a href="{base}/docs/api/opening-a-database/">Start with “Opening a database” →</a></p>

<h2>SQL</h2>
<p>
  What the SQL itself does — types, tables, queries. This is identical no matter which language you
  embed from, so there's no language selector here. Instead, every example is a
  <strong>live database running in your browser</strong>: edit the SQL and run it.
</p>
<p><a href="{base}/docs/sql/types/">Explore the type system →</a></p>

<h2>Reference</h2>
<p>
  Generated tables, straight from jed's canonical spec data: the
  <a href="{base}/docs/reference/types/">type set</a>, the
  <a href="{base}/docs/reference/functions/">functions &amp; operators</a>, and the
  <a href="{base}/docs/reference/errors/">error codes</a>.
</p>
