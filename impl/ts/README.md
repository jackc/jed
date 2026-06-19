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

## Load-bearing decisions

- **Uniform `bigint`.** Every integer (i16/i32/i64) is stored as a JS `bigint`,
  mirroring "everything is i64 internally" in Rust/Go. Exact at all widths; the declared
  column type governs only range checks (`22003`) and key-encoding width — never the
  in-memory representation. `number` is never used for a stored value.
- **UTF-8, not UTF-16.** Table/column names go through `TextEncoder`/`TextDecoder`;
  on-disk `name_len` fields are UTF-8 **byte** lengths, not `String#length`.
- **Big-endian via `DataView`** (`littleEndian = false`), with `setBigUint64` for the u64
  `txid` — never host byte order. CRC-32/IEEE is hand-rolled (`>>> 0` for unsigned),
  pinned by `crc32("123456789") === 0xCBF43926`.
- **No iteration-order leak.** JS `Map` is insertion-ordered, so storage sorts on
  iteration; encoded keys are held as a binary string (byte == code unit) so the default
  string sort is exactly unsigned byte order (CLAUDE.md §8).

## Toolchain (no build step)

Targets modern Node (≥ 22.18 / ≥ 23.6; pinned to `node 24` in `../../mise.toml`) and runs
`.ts` files directly via **native type-stripping** — no emit. The TS is limited to the
**erasable subset** (`erasableSyntaxOnly`): no `enum`, no runtime `namespace`, no
constructor parameter properties; string-literal unions and discriminated unions instead.
Relative imports carry an explicit `.ts` extension.

Dev-dependencies are **type-check only** (no runtime footprint): `typescript` and
`@types/node`. The engine, the unit tests, and the conformance harness all run on **bare
Node with no `npm install`**.

```sh
mise exec -- node src/bin/conformance.ts   # run the shared conformance corpus (no install)
mise exec -- node --test tests/*.test.ts   # unit tests (no install)
mise exec -- npm install && mise exec -- npx tsc --noEmit   # type-check (the one install)
```

Spec data tables (e.g. `spec/encoding/integers.toml`) are read in tests by a tiny
hand-written reader ([tests/tomlmini.ts](tests/tomlmini.ts)) — TOML stays test-time only,
no runtime dependency (CLAUDE.md §5).

> Status: full parity with the Rust and Go cores — every capability and conformance suite,
> and the byte-exact on-disk format (reads the shared goldens and writes byte-identical
> output — the §8 cross-core honesty test, `rust == go == ts == ruby`).
