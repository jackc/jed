# The engine (name TBD)

An **embedded SQL database** — *like SQLite, but with a real type system.* Single-file
storage, a strict static type system, implemented natively in multiple languages in
lockstep with **no reference implementation**.

## Read this first

- **[CLAUDE.md](CLAUDE.md)** — the Project Design Brief. The standing, load-bearing
  record of every architectural decision. Read it before making changes; when a decision
  changes, update it in the same change.
- **[spec/](spec/)** — the **canonical** language-neutral specification and conformance
  corpus. This, not any implementation, is the source of truth (CLAUDE.md §2).

## Repository shape

```
spec/        CANONICAL source of truth — design docs + data tables + conformance corpus
impl/        native cores, one per language (Rust first, then Go), each a downstream
             consumer of spec/
```

## Build order & current status (CLAUDE.md §11)

1. ✅ **Scaffold** the repo around `spec/`.
2. ✅ **Type-system spec** — scalar set + comparison/coercion matrix as data. *Step-1
   scope: signed integers only* (`int16`/`int32`/`int64`). See [spec/types/](spec/types/)
   and [spec/design/types.md](spec/design/types.md).
3. ✅ **Conformance harness format + first corpus tier** — sqllogictest-style format,
   tier/capability-flag system, integer corpus (tiers 1–3). See
   [spec/conformance/](spec/conformance/) and [spec/design/conformance.md](spec/design/conformance.md).
4. ⬜ Storage seam + key-encoding fixtures.
5. ⬜ First vertical slice (`CREATE TABLE` / `INSERT` / `SELECT ... WHERE pk = $1`,
   integer columns only) through both the Rust and Go cores.
