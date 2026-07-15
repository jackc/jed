# Cold point-lookup optimization handoff

Status: P0, P1, and P2 improvement 1 implemented and verified 2026-07-15; P2 improvements 2--4
remain follow-ons. Written 2026-07-14 on
branch `perf/point-lookup-cache` at
`cbbdd184132649a894f72bc58d7be7e2f2d12f87`, then revised after evaluating standard-library,
third-party, SIMD/intrinsic, assembly, alternate-CRC, and cryptographic-hash options.

The immediate task is to remove the CPU-heavy page-checksum implementation from the cold leaf-fault
path without changing the CRC result or weakening corruption detection. Keep CRC-32/IEEE and the
existing file bytes, but select the fastest conforming implementation available in each runtime:
Go's `hash/crc32`, Node's `node:zlib.crc32`, and a safe hand-rolled slicing-by-8 fallback for Rust and
browser TypeScript. The language-neutral contract requires identical checksum parameters, coverage,
bytes, errors, and corruption timing; it does not require identical implementation machinery.

## Repository state and precautions

Read `CLAUDE.md`, `AGENTS.md`, `spec/design/storage.md`, `spec/design/pager.md`,
`spec/design/packed-leaf.md`, `spec/fileformat/format.md`, `spec/design/benchmarks.md`, and `TODO.md`
before changing the design or code.

At handoff time these unrelated working-tree changes already existed and must be preserved:

```text
 M bench/ruby/lib/bench.rb
 M bench/ruby/test/vectors_test.rb
 M impl/rust/src/file.rs
 M impl/wasm/README.md
 M spec/design/api.md
 M spec/design/spill.md
```

All temporary checksum experiments described below were removed, and the normal Rust release
benchmark binary was rebuilt. `impl/rust/src/format.rs` has no residual diagnostic diff.

## Symptom

The relevant result is `point_lookup_pk_ramp (large)` in
`bench/results/20260714-161642/report.md`. It runs:

```sql
SELECT id, customer_id, amount, note FROM orders WHERE id = $1
```

over one million rows, with 2,000 random warmup probes followed by 50,000 measured probes. The same
query's fully-hot lane uses 50,000 warmup probes.

The report shows a bimodal distribution:

| Core | ramp p50 | ramp p90 | ramp p99 | fully-hot p50 | fully-hot p90 |
|---|---:|---:|---:|---:|---:|
| Rust | 2.1 us | 34.7 us | 36.9 us | 2.0 us | 2.2 us |
| Go | 2.1 us | 51.2 us | 67.2 us | 1.9 us | 2.4 us |
| TypeScript | 6.7 us | 62.3 us | 71.6 us | 5.9 us | 7.0 us |

This branch improved the hot path. It did not introduce the cold tail; it made the pre-existing tail
more visible.

## Why p90 lands on a leaf fault

The million-row table occupies roughly 6,900 packed leaves. With uniform random probes, 2,000 warmup
lookups touch only approximately:

```text
6900 * (1 - exp(-2000 / 6900)) ~= 1,740 distinct leaves
```

That leaves about 5,160 first faults during the measured phase. `5160 / 50000 ~= 10.3%`, so the slow
population begins at almost exactly p90. After 52,000 total random probes, nearly every leaf has been
touched, which is why the fully-hot lane collapses back to the 2--7 us steady-state path.

The default jed leaf cache is 256 MiB, or 32,768 leaves at the database's 8 KiB page size. It can hold
this entire working set. This is population, not eviction or CLOCK thrashing; increasing the cache
will not fix the first-touch latency.

The benchmark is cold in jed's application-level leaf cache, not necessarily cold in the OS page
cache. The database files were OS-cache-hot during diagnosis. True storage-cold latency would add
device I/O to both engines and is a different benchmark.

## Root cause

On a leaf-cache miss, jed:

1. allocates and reads one 8 KiB page;
2. validates CRC-32/IEEE over the full page except the four-byte checksum field;
3. validates and retains the PAX directories;
4. inserts the packed leaf into the buffer pool;
5. searches the leaf and reconstructs the touched row columns.

Step 2 dominates. Every core implements `crc32Update` bit by bit: for every byte, it performs eight
polynomial iterations. An 8 KiB leaf therefore executes about 65,000 inner CRC iterations before the
lookup can continue.

Current code locations:

- Rust: `impl/rust/src/format.rs`, `crc32_update` around line 281, `page_crc` around line 304,
  `parse_page` around line 3282, and `decode_leaf_node` around line 3571.
- Go: `impl/go/format.go`, `crc32Update` around line 218, `pageCRC` around line 240,
  `parsePage` around line 3554, and `decodeLeafNode` around line 3818.
- TypeScript: `impl/ts/src/format.ts`, `crc32Update` around line 296, `pageCrc` around line 318,
  and `parsePage` around line 3127.
- The Rust file host also performs `seek` plus `read_exact` in `impl/rust/src/blockstore.rs` around
  lines 70--74. This is secondary, not the main problem.

The CRC behavior is intentional. `format_version` 7 added a CRC-32/IEEE to every body page so a
corrupted live page fails with `XX001` when parsed. See `spec/design/storage.md` around lines 305--320
and `spec/fileformat/format.md` around lines 493--495 and 516--535. Removing or optionally bypassing
the CRC would weaken a standing integrity guarantee and is not the fix.

SQLite wins despite doing more OS reads. A syscall-counting run observed roughly 6,900 jed database
page reads over the complete ramp run versus roughly 101,000 SQLite `pread64` calls. Those counts are
reliable even though `strace` heavily distorted the measured wall times. SQLite's ordinary file format
does not validate an equivalent whole-page checksum on each page-cache miss, so its OS-cache-hit reads
remain cheap.

## Controlled Rust evidence

Three release runs per variant were made against the same dataset and parameter stream. All variants
retained checksum `f82d3b99ddaff0fb`. Values below are representative medians of the three runs.

| CRC path | mean | p50 | p90 | p99 |
|---|---:|---:|---:|---:|
| Current bit-at-a-time CRC | 5.73 us | 2.25 us | 35.0 us | 37.5 us |
| CRC validation temporarily bypassed | 2.59 us | 2.17 us | 4.80 us | 7.28 us |
| One 256-entry table, one byte/step | 3.75 us | 2.33 us | 15.4 us | 17.2 us |
| Safe slicing-by-8 prototype | 2.92 us | 2.18 us | 7.94 us | 10.2 us |

The bypass was diagnostic only. It proves that about 30 us of every cold Rust lookup is checksum CPU,
not query planning, B+tree descent, row reconstruction, or OS I/O. The one-table version removed more
than half of that cost. Slicing-by-8 removed roughly 80% of the excess p90 while preserving the exact
CRC vector and successfully opening and querying the existing format-v29 database.

The slicing-by-8 prototype used eight derived 256-entry tables (8 KiB total), processed an eight-byte
chunk with `u32::from_le_bytes` plus eight table reads, and used the ordinary table update for the
remainder. It was entirely safe Rust: no `unsafe`, intrinsics, dependency, assembly, or format change.

## Checksum-backend evaluation

Focused local microbenchmarks were run on an Intel Core Ultra 9 285K with PCLMUL, AVX-512, and SHA
extensions. They are directional backend measurements rather than substitutes for the end-to-end
point-lookup ramp:

| Runtime/backend | Approximate time per 8 KiB page checksum |
|---|---:|
| Go `hash/crc32`, IEEE | 0.16 us (projected from 4/32 KiB throughput) |
| Node `node:zlib.crc32`, IEEE | 1.1 us (exact split-page coverage) |
| Go SHA-256 | 1.7 us |
| Go IEEE with PCLMUL disabled | 2.1 us (projected) |
| Node SHA-256 | 2.4 us |
| Go / Node SHA-1 | 3.1 / 3.7 us |
| Pure TypeScript slicing-by-8 | 5.7 us |
| Go / Node MD5 | 6.9 / 7.5 us |
| Current pure TypeScript bit-at-a-time CRC | 74.6 us |

The results are host-specific, but the backend decision is portable because the standard libraries
perform their own runtime dispatch and retain software fallbacks:

- Go's `hash/crc32` accelerates CRC-32/IEEE with PCLMUL assembly on suitable amd64 systems and has
  optimized architecture-specific or slicing-by-8 paths elsewhere. Use the standard library instead
  of duplicating it in Go.
- Node exposes the same CRC as the stable `node:zlib.crc32(data, previous)` API. It is approximately
  five times faster than the safe pure-TypeScript slicing prototype on this host. Use it only through
  the Node host path so the browser/OPFS import graph remains free of `node:*` modules.
- Browsers have no synchronous standard CRC API. WebCrypto digests are asynchronous and non-streaming,
  which does not fit the synchronous page-parse and block-store seam. Browser TypeScript therefore
  needs a pure implementation.
- Hardware-backed SHA-256 can beat the current naive CRC and can beat MD5 and SHA-1, but it remains
  slower than optimized CRC-32/IEEE. A full digest enlarges the page header; truncating it to 32 bits
  gives no better random-collision width than CRC-32 and loses CRC's structured-error guarantees. An
  unkeyed digest also does not authenticate against deliberate rewriting.
- CRC32C/Castagnoli would require a format change and would lose Node's standard IEEE accelerator.
  On this host Go's IEEE path was about twice as fast as its CRC32C path. For the current 8 KiB page,
  CRC-32/IEEE already has Hamming distance 4, detecting all one-, two-, and three-bit errors, as well
  as all bursts through 32 bits. A custom polynomial could strengthen some bounds but would forfeit
  the standard fast paths; neither change is a cold-performance improvement.
- Moving the checksum field to make coverage contiguous saved only about 0.03 us per Node page versus
  two incremental calls. That does not justify a format revision.
- Go's experimental SIMD package is unnecessary: it is opt-in and unstable, while `hash/crc32`
  already contains mature runtime-dispatched assembly and fallbacks.
- Rust intrinsics or hand-written assembly would add `unsafe`, target-specific implementations, and
  a portable fallback. If safe slicing-by-8 leaves a material Rust bottleneck, benchmark the existing
  `crc32fast` and `crc-fast` crates before considering jed-owned intrinsics or assembly. Both use unsafe
  SIMD internally; `crc-fast` also needs default FFI features disabled. Either dependency requires
  explicit human approval and a deliberate decision about the repository's safe-core policy.

## P0: preserve CRC-32/IEEE and use per-runtime fast paths — complete

Replace the bit-at-a-time production path in every core, but do not require the mechanical algorithm
to match across languages. Keep all of these observable facts exactly unchanged:

- reflected CRC-32/IEEE polynomial `0xEDB88320`;
- initial register `0xFFFFFFFF`;
- final XOR `0xFFFFFFFF`;
- incremental composition over disjoint spans;
- page coverage `[0, 12)` followed by `[16, page_size)`;
- known vector `crc32("123456789") == 0xCBF43926`;
- all existing file bytes, goldens, errors, and corruption timing.

Implementation plan by core:

### Rust

Use the already measured safe slicing-by-8 design: a `const fn` that derives
`[[u32; 256]; 8]` from the polynomial, one stored `const`, safe indexed chunk reads, and the ordinary
table update for the remainder. Do not add a dependency, intrinsic, or assembly in P0.

### Go

Delete the private bit-at-a-time updater and use the standard `hash/crc32` package with
`crc32.IEEETable`. Use `crc32.ChecksumIEEE` for a contiguous checksum and the public finalized-state
composition API for a page:

```go
checksum := crc32.Update(0, crc32.IEEETable, page[:12])
checksum = crc32.Update(checksum, crc32.IEEETable, page[16:])
```

Do not add a Go dependency, copy the standard library's tables, call experimental SIMD directly, or
write jed-owned Go assembly.

### TypeScript

Create one narrow internal CRC-32/IEEE backend contract that supports finalized incremental
composition, with a safe slicing-by-8 implementation as the browser-neutral default. Derive eight
`Uint32Array(256)` tables from the polynomial during module initialization and normalize signed
JavaScript bitwise results with `>>> 0`.

For the Node host, install/select a backend implemented with the stable `crc32` export from
`node:zlib`; pass the first span's returned checksum as the second call's `value`. Select the backend
once during host initialization, before opening or creating a database. Keep `node:zlib` imports in a
Node-only module/entry path: `format.ts`, `opfs.ts`, the worker, and their transitive browser graph must
remain free of `node:*` imports. Run the existing browser typecheck/build checks to enforce the
boundary. Backend selection must not be exposed as a user option, remain mutable after initialization,
or make checksum behavior depend on import order. Ensure every supported Node entry path selects the
Node backend; direct browser/core entry paths must deterministically retain the pure fallback.

The tables used by the Rust and browser implementations are derived machinery, not new spec data.
The polynomial, initialization/finalization rules, coverage, byte order, and known result remain
canonical in `spec/fileformat/format.md`.

Update `spec/fileformat/format.md` in the same change. Replace the statement that CRC is "hand-rolled
identically in every core (no runtime dependency)" with a language-neutral output contract permitting
standard-library and runtime-accelerated implementations. This is an implementation clarification,
not a byte change, so it requires no format-version bump or golden regeneration.

### Tests for P0

Retain the existing per-core known-vector tests:

- Rust: `impl/rust/src/format.rs::crc32_known_vector`.
- Go: `impl/go/fileformat_golden_test.go::TestCRC32KnownVector`.
- TypeScript: `impl/ts/tests/fileformat_golden.test.ts`, `crc32 known vector`.

Add a test of finalized incremental composition because page checksumming invokes the backend on two
disjoint spans. For deterministic payloads and every split position, assert the equivalent of:

```text
update(update(0, bytes[0..split]), bytes[split..]) == checksum(bytes)
```

The exact raw-register helper may remain an implementation detail where useful, but tests at the
backend boundary should use the finalized checksum state accepted by Go and Node's public APIs.

Keep a slow bit-at-a-time implementation under test-only compilation as a simple independent oracle
for the Rust and pure-TypeScript slicing backends. Compare it with the selected backend over
deterministic lengths, alignments, split points, and page-sized buffers. In Node, run the same vectors
against both the zlib backend and pure fallback. Do not retain the slow function in a production hot
path.

Add or retain tests that corrupt a byte in each protected page region, verify the stored checksum
field itself is excluded, and confirm that opening/touching the corrupt page still fails with `XX001`
at the same point. Existing cross-core golden files must open without rewriting.

Required verification should include:

```sh
cargo test --release --manifest-path impl/rust/Cargo.toml
(cd impl/go && go test ./...)
(cd impl/ts && npm run --silent test)
(cd impl/ts && npm run --silent typecheck:browser)
(cd impl/ts && npm run --silent build:browser)
rake verify
rake 'bench:run[point_lookup_pk_ramp]'
rake 'bench:run[point_lookup_pk]'
```

Run `rake ci` before integration. The benchmark runs are observational; unchanged goldens,
corruption tests, cross-core checksums, and conformance are the correctness gates.

Record before/after mean, p50, p90, and p99 for every native core in `spec/design/benchmarks.md`, along
with the selected backend and relevant runtime/CPU feature context. Add/update the corresponding perf
follow-on in `TODO.md`. This optimization changes no SQL behavior, host API, deterministic cost, or
file bytes, so it needs no website update or format-version bump.

### P0 completion and Rust escalation gate

P0 is complete only after all three cores preserve the byte contract and the end-to-end native ramp
has been rerun. If Rust's residual checksum time remains material after safe slicing-by-8, make a
separate explicit decision before adding a dependency or relaxing the safe-core posture:

1. benchmark `crc32fast` and `crc-fast` against the same page coverage and end-to-end ramp;
2. include dispatch overhead, non-PCLMUL fallback behavior, supported architectures, dependency
   features, unsafe/FFI inventory, and maintenance risk;
3. prefer a vetted crate over jed-owned intrinsics or hand-written assembly if the policy exception
   and measured gain are both accepted;
4. otherwise retain safe slicing-by-8.

Completion result (2026-07-15): all three core suites, browser typecheck/build, `rake verify`, and
the native ramp/hot benchmarks passed. Ramp mean / p50 / p90 / p99 was
2.953/2.208/5.543/9.215 us Go, 2.926/2.103/7.566/10.882 us Rust, and
8.548/6.885/11.931/20.521 us TypeScript. Checksums remained `f82d3b99ddaff0fb` (ramp) and
`28f09c46d56e242a` (hot). The safe Rust result meets the target closely enough that the optional
dependency/SIMD escalation is not scheduled.

## P1: reduce residual first-fault parsing and allocation — complete

After P0, the Rust no-CRC diagnostic floor was about 4.8 us p90 and slicing-by-8 was about 7.9 us.
The remaining cold work includes the page allocation/read and PAX directory validation/allocation.

`parse_pax_leaf` currently builds owned integer vectors for the key end offsets and every variable
column's end offsets. Consider retaining zero-copy directory ranges into the packed page and reading
big-endian `u32` offsets directly on access, while still performing the complete ascending/bounds
validation once at fault time. This should:

- remove several per-leaf allocations;
- reduce Go and TypeScript GC pressure;
- make resident memory closer to exactly one page per cached leaf;
- preserve the existing corruption timing and packed-leaf semantics.

Do not defer or skip directory validation merely for speed; the current fail-closed behavior is part of
the corruption contract.

Completion result (2026-07-15): Rust and TypeScript retain numeric payload offsets and Go retains
`[]byte` page views for the key and variable-value end-offset directories. All three cores still scan
and validate every directory entry during the leaf fault, then read individual big-endian offsets in
O(1) on access. This removes `1 + V` owned `N`-entry integer arrays per faulted leaf with `V`
variable-width columns. Dedicated corruption tests cover descending key and value directories and
require fault-time `XX001`; representation tests pin the page-backed storage. The final shared run
(`bench/results/20260715-042240`) measured ramp mean / p50 / p90 / p99 at
2.942/2.150/4.896/9.993 us Go, 2.798/2.077/7.527/9.586 us Rust, and
7.960/6.631/10.925/15.995 us TypeScript. Ramp/hot checksums remained
`f82d3b99ddaff0fb` / `28f09c46d56e242a`.

## P2: smaller residual and tail improvements

These are secondary and should be measured only after P0/P1:

1. **Complete (2026-07-15):** Rust and Go give the buffer-pool page-id index a bounded initial
   capacity hint of `min(cache_leaves, 8192)`. This covers the diagnosed roughly 6,900-leaf population
   without turning the default 32,768-leaf ceiling or a caller's larger ceiling into an unbounded eager
   allocation. Focused before/after population probes moved Go from 361.5 to 200.2 us, 69 to 51
   allocations, and 1,042,895 to 1,010,336 allocated bytes; Rust moved from 239.6 to 133.8 us and 12
   index growths to zero. Five-run median end-to-end ramp movements stayed within the 5% noise floor
   and retained checksum `f82d3b99ddaff0fb`; full results are in `spec/design/benchmarks.md`.
2. Use safe positioned reads for the Rust file host where the standard library supports them, avoiding
   the current `seek` plus `read_exact`. Keep a correct portable fallback. The syscall trace indicates
   this is a small single-reader improvement.
3. For concurrent cold faults, stop holding the global pool lock across file read, checksum, and PAX
   parse. The current Rust path holds the pool mutex through the loader; Go uses one mutex for pager and
   pool. Use a per-page loading/single-flight state or an unlock-load-recheck insertion protocol, while
   preserving commit synchronization and preventing stale page-id reuse. This does not explain the
   single-reader ramp result, but the slow CRC amplifies head-of-line blocking for concurrent readers.
4. If applications need predictable startup latency and the working set fits in the configured cache,
   an explicit/background prewarm facility can shift first faults out of request latency. Treat it as
   a policy feature, not a substitute for making faults cheap.

## Approaches that do not solve the cause

- Raising the default cache: this benchmark already fits entirely in the 256 MiB cache.
- Longer benchmark warmup: that merely hides first-touch cost; the separate hot lane already measures
  steady state.
- Changing the page size: it trades checksum bytes against B+tree fan-out and changes the file's
  creation-time layout; it is not the right fix for an inefficient CRC loop.
- Disabling checksums or adding a checksum-off open option: this weakens documented at-rest corruption
  detection.
- Substituting the x86 SSE4.2 `crc32` instruction in the existing format: it computes
  CRC32C/Castagnoli, not CRC-32/IEEE, and therefore produces wrong file bytes. A deliberate CRC32C
  format revision was evaluated above and is not justified by integrity or performance here.
- Replacing the four-byte CRC with MD5, SHA-1, SHA-256, xxHash, Adler, or Fletcher solely for speed:
  each is either slower than the available IEEE fast paths, weaker for relevant structured errors,
  wider on disk, unavailable synchronously across all targets, or dependent on new third-party code.
- Writing jed-owned SIMD/intrinsics/assembly before measuring the safe Rust implementation and vetted
  crates: this adds unsafe, architecture, and maintenance burden before establishing a remaining need.
- Adding a CRC dependency without explicit approval and an accepted safe-core policy decision:
  prohibited by the dependency policy and unnecessary for P0's measured win.

## Outcome and remaining work

P0 achieved the expected tail collapse: Rust ramp mean is roughly 3 us with p90/p99 around 8/11 us,
Go's standard library reaches about 3 us mean and 6/9 us p90/p99, and Node's zlib-backed TypeScript
reaches about 9 us mean and 12/21 us p90/p99. Fully-hot performance stayed effectively unchanged.
The browser fallback remains pure TypeScript and should be measured separately on each supported
browser when browser performance becomes an active benchmark lane.

P1 removed the residual per-record directory allocations without weakening validation. The smaller P2
work can target remaining page-read and cache-population tails without weakening the checksum guarantee.
