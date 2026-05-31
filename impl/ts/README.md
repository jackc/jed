# impl/ts/ — the TypeScript core (later)

A later consumer of the already-hardened spec (CLAUDE.md §2). Must run in the **browser**
(storage seam backed by OPFS, CLAUDE.md §9).

Under the multi-core philosophy a **native TypeScript implementation is the default**; a
Rust→WASM wrapper remains a pragmatic fallback. **TBD** — decided when we get here, not
now.

> Status: placeholder. Not started; comes after the Rust and Go cores are hardened.
