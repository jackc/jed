# impl/ts/ — the TypeScript core

The third native core (CLAUDE.md §2), built **natively** — *not* a Rust→WASM wrapper.
It exists to exercise dimensions the two systems cores share and hide: JS has no native
i64 (numbers are f64), strings are UTF-16 (the format is UTF-8), and there is no native
big-endian access. A third independent implementation that still produces **byte-identical**
output is the strongest confirmation the spec is real, not a Rust/Go accident (CLAUDE.md §2, §8).

This core is a **consumer of [../../spec/](../../spec/)**, not an author of it. It mirrors
the Go core module-for-module; the only differences are idioms (throw `EngineError` rather
than return an error; discriminated unions rather than one-field-set structs).

Style (CLAUDE.md §10): **boring, explicit code over clever abstraction.**

## Async application API

The `jed-ts` entry point executes SQL in a long-lived Node worker. `jed-ts/browser` uses
a dedicated browser worker and OPFS (or memory when no path is supplied). Both expose the
same async database/session/transaction/prepared/cursor lifecycle:

```ts
import { createDatabase } from "jed-ts"; // "jed-ts/browser" in a browser bundle

const db = await createDatabase({ path: "app.jed" });
try {
  await db.execute("CREATE TABLE items (id i64 PRIMARY KEY, title text)");
  await db.update(async (tx) => {
    const insert = await tx.prepare("INSERT INTO items VALUES ($1, $2)");
    try { await insert.run(1n, "first"); }
    finally { await insert.close(); }
  });
  const rows = await db.query("SELECT * FROM items ORDER BY id");
  for await (const row of rows) console.log(row); // lossless Value[]; early exit closes
} finally {
  await db.close();
}
```

`query` yields typed values, including rehydrated decimals, exact integers, bytes and recursive
containers. `get`/`all` return the existing ergonomic JS object mapping. A prepared statement's
`query(params)`/`execute(params)` use `Value[]`; `run/get/all(...params)` accept native JS values.
`batch` amortizes messaging without adding a transaction; `executeScript` is atomic.
Database convenience calls use fresh sessions; use `await db.session()` to keep variables,
temporary tables and explicit SQL transaction state. Close every session and prepared statement.

`runtime` options bound outstanding requests, batches, client result memory, handles and worker
count. Defaults and the migration contract are in [ts-runtime.md §10](../../spec/design/ts-runtime.md#10-implemented-async-host-contract).
A single oversized row or `all` collection fails `TransportError("RESULT_TOO_LARGE")`; consume
large results with async iteration. SQL errors remain `EngineError` with SQLSTATE and diagnostics.
`WORKER_FAILED` with `outcome: "unknown"` means a lost response may follow a committed write;
requests are never automatically replayed.

Pass `{signal}` to `query`/`execute` for cancellation. Node and cross-origin-isolated browsers
poll shared flags during metered work. Ordinary browsers cancel between operations and row
batches; a currently executing synchronous blocking kernel cannot yet process cancel messages.
Async page/crypto suspension and universal cooperative cancellation remain follow-ons.

Node's `extensionModule` accepts a trusted file URL exporting `configure(builder)`. That module
registers host functions on the worker's `EnvironmentBuilder` before storage opens. Application
closures are not transferable. Unicode/time-zone bundles may be passed as data at open.

The repository's synchronous core (`src/lib.ts`, also `jed-ts/internal`) remains available for
conformance and existing host tooling. It does not offload work; application code uses the async
entry points. No new runtime dependency or database format was introduced by this migration.

## Load-bearing decisions

- **Uniform `bigint`.** Every integer (i16/i32/i64) is stored as a JS `bigint`,
  mirroring "everything is i64 internally" in Rust/Go. Exact at all widths; the declared
  column type governs only range checks (`22003`) and key-encoding width — never the
  in-memory representation. `number` is never used for a stored value.
- **UTF-8, not UTF-16.** Table/column names go through `TextEncoder`/`TextDecoder`;
  on-disk `name_len` fields are UTF-8 **byte** lengths, not `String#length`.
- **Big-endian via `DataView`** (`littleEndian = false`), with `setBigUint64` for the u64
  `txid` — never host byte order. CRC-32/IEEE is pinned by
  `crc32("123456789") === 0xCBF43926`; Node uses `node:zlib.crc32`, while browser/OPFS paths use a
  safe slicing-by-8 implementation (`>>> 0` keeps results unsigned). Both produce identical bytes.
- **No iteration-order leak.** JS `Map` is insertion-ordered, so storage sorts on
  iteration; encoded keys are held as a binary string (byte == code unit) so the default
  string sort is exactly unsigned byte order (CLAUDE.md §8).
- **Independent engine, narrow native Node lock host.** SQL, storage, types, execution, and file
  bytes are TypeScript. Node's file host loads the `jed_lock.node` artifact built from
  `native-lock/` solely for
  crash-clean whole-file OS locks (presence ownership and arrival) because Node exposes no standard
  `flock`/`LockFileEx`; queries make no foreground addon call. Browser/OPFS and in-memory paths do
  not import it.

## Toolchain (no build step)

Targets modern Node (≥ 22.18 / ≥ 23.6; pinned to `node 24` in `../../mise.toml`) and runs
`.ts` files directly via **native type-stripping** — no emit. The TS is limited to the
**erasable subset** (`erasableSyntaxOnly`): no `enum`, no runtime `namespace`, no
constructor parameter properties; string-literal unions and discriminated unions instead.
Relative imports carry an explicit `.ts` extension.

The engine has **one runtime dependency**, the exact-pinned `@noble/hashes`, which supplies the
digest functions `md5` / `sha224` / `sha256` / `sha384` / `sha512` (CLAUDE.md §14 clause 3: no
hand-rolled crypto). It is pure JavaScript with no dependencies of its own and runs in Node and the
browser. The dev-dependencies are **type-check and browser-test only**: `typescript`,
`@types/node`, `vite`, and `@playwright/test`. The engine, the unit tests, and the conformance
harness run on bare Node after one `npm ci`, with no build step. File-backed Node use additionally
needs the matching first-party prebuilt lock artifact; repository tests build it from exact-pinned
safe Rust sources with `mise run ts:lock_build`, which also runs that `npm ci`.

```sh
mise exec -- npm ci                        # install @noble/hashes (+ the dev tools)
mise exec -- node src/bin/conformance.ts   # run the shared conformance corpus
mise exec -- node --test tests/*.test.ts   # unit tests
mise exec -- npx tsc --noEmit              # type-check
mise run ts:lock_build                     # build the local Node OS-lock host
```

Spec data tables (e.g. `spec/encoding/integers.toml`) are read in tests by a tiny
hand-written reader ([tests/tomlmini.ts](tests/tomlmini.ts)) — TOML stays test-time only,
no runtime dependency (CLAUDE.md §5).

> Status: full parity with the Rust and Go cores — every capability and conformance suite,
> and the byte-exact on-disk format (reads the shared goldens and writes byte-identical
> output — the §8 cross-core honesty test, `rust == go == ts == ruby`).
