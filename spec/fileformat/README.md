# spec/fileformat/ — on-disk format + byte fixtures

The single-file on-disk format (CLAUDE.md §9), specified with **byte-exact fixtures**. The
load-bearing conformance test: a database file written by the Rust core must be
byte-readable by the Go core and vice versa (CLAUDE.md §8). That one round-trip catches an
entire class of cross-implementation divergence automatically.

Storage design targets **in-RAM datasets with SSD-backed persistence** (CLAUDE.md §9):
the in-memory representation is first-class, and on-disk layout/block size are chosen for
SSD characteristics. Writes batch in a private staging area and land at commit (CLAUDE.md
§3).

The **storage architecture** — the block-device seam, the page model, and the
root-pointer-swap commit model that carries CLAUDE.md §3 — is designed in
[../design/storage.md](../design/storage.md). *This* directory holds the concrete **byte
format** that realizes it.

## Files

| File | Contents |
|---|---|
| [format.md](format.md) | The byte-exact format: meta double-buffer, page header, catalog chain, record layout, value codec, stable type codes, packing rule. The canonical contract. |
| [fixtures/](fixtures/) | Byte-exact golden `.adb` files at `page_size = 256` (reviewable hex). Each core reads them and writes bytes equal to them. |
| [verify.rb](verify.rb) | Independent Ruby reference that (re)generates and validates the goldens — a *third* implementation, so the goldens are not self-certified by the two cores. `--generate` rewrites them; bare run verifies. Test-time only; run via `rake verify`. |

## Scope (step-5b)

The current format is **whole-image**: a commit serializes the entire database to one
byte image (data is RAM-first — CLAUDE.md §9). Incremental copy-on-write, free-list / page
reclamation, and B-tree interior pages are deliberately **deferred** until `UPDATE`/`DELETE`
create the pressure that justifies them (CLAUDE.md §11). The double-meta page + root pointer
are the forward-looking hooks for the live commit model ([../design/storage.md](../design/storage.md) §4).

> Status: format authored ([format.md](format.md)); 6 byte-exact fixtures generated and
> verified by the independent Ruby reference; the Rust and Go cores both read every golden
> and write byte-identical output (the CLAUDE.md §8 cross-core round-trip). Composite-key,
> non-integer, and incremental-commit format details follow when those features land.
