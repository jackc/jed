<svelte:head>
	<title>Installation — jed</title>
	<meta
		name="description"
		content="Install and run jed: try it live in your browser, or embed the pure-Go core with go get. A 0.x preview."
	/>
</svelte:head>

# Installing & running jed

> ⚠️ jed is a **0.x preview and is not ready for production use.** Expect changes to behavior and to
> the on-disk file format between releases — see [Preview status](../status/) before you store
> anything you can't reproduce.

## Try it without installing anything

The fastest way to see jed is the **[live playground](../../tool/)**. The engine compiles to run entirely
in your browser — a native TypeScript core in a Web Worker, with databases in memory or in your
browser's origin-private file system (OPFS) — so nothing is sent to a server. Every example on the
[SQL docs](../sql/types/) pages is editable and runnable the same way.

## Embed it in Go

The Go core is **pure Go** — no cgo, no FFI — so it installs with no native toolchain:

```sh
go get github.com/jackc/jed/impl/go@latest
```

Because the import path's last element is `go`, Go imports the package under an alias (`jed`):

```go
import jed "github.com/jackc/jed/impl/go"
```

From there, open or create a single-file database, run SQL, and commit. See
**[Opening a database](../api/opening-a-database/)** for the full, runnable example, and the rest
of the [Embedding API](../api/transactions/) pages for transactions, scripts, authorization, and
resource limits.

## The other cores

jed is also implemented natively in **Rust** and **TypeScript**, and wrapped for **Ruby** and
**WebAssembly**. Today these build from source in the
[repository](https://github.com/jackc/jed), but are **not yet published** to crates.io, npm, or
RubyGems — that comes in a later release. For now, Go (`go get`) and the in-browser playground are
the supported ways to run jed.
