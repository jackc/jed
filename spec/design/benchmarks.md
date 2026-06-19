# Benchmarks — cross-core and cross-engine wall-clock measurement

Status: v1 landed (corpus format, setup tool, six benchmarks, harnesses in all three
cores). This document is the canonical record for the `bench/` subsystem.

## 1. Purpose and non-goals

The benchmark suite answers two questions, continuously:

1. **Cross-core** — how do the three jed cores (Rust, Go, TS) compare against each other
   on the same workload?
2. **Cross-engine** — is jed's performance *at least tolerable* next to PostgreSQL and
   SQLite on the same workload?

This is **wall-clock** measurement and therefore deliberately **outside the conformance
contract and `rake ci`** (CLAUDE.md §10): timings are environment-relative and
nondeterministic, and must never gate a build. What *is* checked — loudly — is the
**answers**: every result carries a checksum of the returned rows, and the report fails
if any two engines/cores/drivers disagree for the same benchmark (§6). A benchmark run
is thus also a small differential test.

Non-goals: micro-benchmarks of internal functions (use `cargo bench`/`testing.B` ad hoc
if needed), benchmarking the deterministic *cost* units (cost is asserted exactly in the
conformance corpus — `spec/design/cost.md`), and *wall-clock* load/concurrency testing
(a backfill candidate once `SharedDb` is file-backed — §11). **Correctness**-under-concurrency
*is* covered, by a sibling bench-family harness: the Layer 3 stress runner
(`spec/design/concurrency-testing.md` §6) shares these modules' machinery (the splitmix64 PRNG,
the FNV-1a answer checksum) via a `stress` binary per core, run by `rake stress` — see §2.

## 2. Layout

```
bench/
  corpus/benchmarks.toml   # benchmark definitions (shared by every harness)
  corpus/datasets.toml     # dataset spec (shared by bench-setup and fingerprint checks)
  go/                      # Go harness module (jed-bench) — own go.mod, never impl/go's
  rust/                    # Rust harness package (jed-bench) — own Cargo.toml, never impl/rust's
  ts/                      # TS harness package — own package.json, never impl/ts's
  go/cmd/stress, rust/src/bin/stress.rs, ts/src/stress.ts   # Layer 3 concurrency-stress runner
                           #   (concurrency-testing.md §6); reuses the PRNG + checksum below
  data/                    # GITIGNORED: generated {small,large}.{jed,sqlite} + *.fingerprint
  results/                 # GITIGNORED: <UTC-stamp>/<lang>-<binary>.jsonl per run (+ stress/)
stress/*.stress.toml       # Layer 3 stress workloads (run by `rake stress`)
scripts/bench_report.rb    # aggregator (rake bench:report)
```

PostgreSQL benchmark data lives in the live `db` service (databases `jed_bench_small`,
`jed_bench_large`, `jed_bench_scratch`), reached over the Unix socket like the oracle
(`PGHOST=/var/run/postgresql`, trust auth).

The corpus is data, the harnesses are code: each harness parses the same two TOML files
and runs the same benchmarks; only the engine driver differs per binary (§7). New
benchmarks are added by editing `benchmarks.toml` (and, if they need new data,
`datasets.toml` + a `generator_version` bump — §5).

## 3. Benchmark corpus — `bench/corpus/benchmarks.toml`

```toml
schema_version = 1

[[bench]]
name        = "point_lookup_pk"     # unique; result key together with (engine, lang, variant)
description = "PK point lookup on 1M rows"
dataset     = "large"               # "small" | "large" | "scratch" (§8)
kind        = "query"               # "query" | "write_rollback" | "write_durable"
sql         = "SELECT id, customer_id, amount, note FROM orders WHERE id = $1"
warmup      = 2000                  # untimed iterations (consume the same param stream)
iterations  = 50000                 # timed iterations
seed        = 4201                  # splitmix64 seed for this bench's param stream (§4)

[[bench.param]]                     # one entry per $N, ascending
gen = "int_uniform"                 # "int_uniform" | "serial" | "text"
min = 1                             # int_uniform: inclusive bounds
max = 1000000
```

Optional keys:

- `expect_rows_per_iter = N` — sanity gate: abort if any measured iteration returns a
  different row count.
- `engines = ["postgres", ...]` — allowlist; default is all three. The escape hatch for
  a benchmark only some engines can run.
- `batch = N` — write kinds only: statements per iteration (§8).
- `setup_sql = ["..."]` — write kinds only: statements run once before warmup.
- `[bench.sql_override]` / `[bench.setup_sql_override]` — per-engine SQL text keyed by
  `jed` / `postgres` / `sqlite`, used only where dialects genuinely diverge (v1 uses it
  once, for the scratch table's SQLite rowid-pk DDL — §8).

Param generators: `int_uniform` (`min`/`max`, drawn as `min + next() % (max-min+1)`),
`serial` (`start`; a monotonic counter that does **not** consume the PRNG — collision-free
ids), `text` (`min_len`/`max_len`; a length draw then per-char draws — §4).

**Placeholder policy.** SQL uses `$N` with first occurrences in ascending order. jed and
PostgreSQL bind `$N` natively; SQLite harnesses mechanically rewrite `$N` → `?N` at
prepare time (`?N` is SQLite's explicit-numbered positional form, unambiguous across
rusqlite, the Go drivers, and `node:sqlite`).

**Ordering rule.** Any benchmark query returning more than one row must carry a
**total-order `ORDER BY`** — ties broken explicitly (by `id`), because jed's implicit
primary-key tie-break (§8 of CLAUDE.md) is *not* shared by PostgreSQL or SQLite and the
checksum (§6) compares rendered rows in order.

**Type rule.** Stick to the common subset: jed's `smallint`/`integer`/`bigint` aliases
parse in all three engines, so one SQL text serves all of them. Aggregates were chosen so
result types match too (jed `SUM(i32) → i64` = PG `sum(integer) → bigint`).

## 4. Dataset spec and deterministic generation — `bench/corpus/datasets.toml`

```toml
schema_version    = 1
generator_version = 1     # BUMP whenever generation behavior changes (part of the fingerprint)

[[dataset]]
name = "small"            # → bench/data/small.{jed,sqlite}, PG database jed_bench_small

[[dataset.table]]
name = "orders"
rows = 10000
seed = 101                # one splitmix64 stream per table

  [[dataset.table.column]]
  name = "id"
  type = "i64"          # i64 | i32 | i16 | text
  gen  = "serial"         # 1..rows; consumes no PRNG draws
  primary_key = true

  [[dataset.table.column]]
  name = "customer_id"
  type = "i32"
  gen  = "int_uniform"
  min  = 1
  max  = 1000

  [[dataset.table.index]]
  name    = "orders_customer_idx"
  columns = ["customer_id"]
  # optional engines = [...] allowlist (future axis); default: every engine gets the index
```

**Generation order is the determinism contract:** for each table, seed one splitmix64
stream with `table.seed`; for each row 1..rows, draw each non-`serial` column's value in
declared column order. Any language reproduces the dataset exactly from the spec.

**DDL is derived** from the spec per engine — never written as literal SQL — with this
fixed type map:

| spec type | jed | PostgreSQL | SQLite |
|---|---|---|---|
| `i64` + `primary_key` | `bigint PRIMARY KEY` | `bigint PRIMARY KEY` | `INTEGER PRIMARY KEY` |
| `i64` / `i32` / `i16` | same name | `bigint` / `integer` / `smallint` | `INTEGER` |
| `text` | `text` | `text` | `TEXT` |

The SQLite pk maps to `INTEGER PRIMARY KEY` (the rowid alias) deliberately: it is
SQLite's idiomatic fast path, and `BIGINT PRIMARY KEY` would unfairly force a separate
index. Fairness notes like this live here so a surprising number has a written
explanation.

### The shared PRNG — splitmix64

State is one u64 `z`. One step, all arithmetic wrapping to 64 bits:

```
next():
  z += 0x9E3779B97F4A7C15
  x = z
  x = (x ^ (x >> 30)) * 0xBF58476D1CE4E5B9
  x = (x ^ (x >> 27)) * 0x94D049BB133111EB
  return x ^ (x >> 31)
```

Draws:

- bounded int in `[lo, hi]`: `lo + next() % (hi - lo + 1)` (modulo bias accepted — it is
  deterministic and identical everywhere, which is all that matters here);
- text of length `[min_len, max_len]`: one bounded draw for the length, then per
  character `'a' + next() % 26`.

Ruby reference (the snippet that generated the pinned vectors below):

```ruby
MASK = 0xFFFFFFFFFFFFFFFF
def splitmix64_stream(seed, n)
  z = seed & MASK
  Array.new(n) do
    z = (z + 0x9E3779B97F4A7C15) & MASK
    x = z
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & MASK
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & MASK
    x ^ (x >> 31)
  end
end
```

Pinned vectors — asserted by a unit test in every harness (Go
`bench/go/internal/bench/prng_test.go`, Rust `bench/rust/src/lib.rs`, TS
`bench/ts/tests/prng.test.ts`; TS implements the PRNG over `BigInt` masked to 64 bits):

| seed | first five outputs (hex) |
|---|---|
| `1` | `910a2dec89025cc1`, `beeb8da1658eec67`, `f893a2eefb32555e`, `71c18690ee42c90b`, `71bb54d8d101b5b9` |
| `1234567` | `599ed017fb08fc85`, `2c73f08458540fa5`, `883ebce5a3f27c77`, `3fbef740e9177b3f`, `e3b8346708cb5ecd` |

## 5. Fingerprints — setup once, regenerate only when stale

`fingerprint = sha256_hex(bytes of bench/corpus/datasets.toml)`. The file embeds
`generator_version`, so a behavioral change in `bench-setup` that doesn't touch the
dataset shape is still a one-line bump that invalidates everything.

Stored per engine after a successful load:

- jed / SQLite: sidecar files `bench/data/<dataset>.jed.fingerprint` /
  `<dataset>.sqlite.fingerprint` (hex + `\n`);
- PostgreSQL: row `('fingerprint', <hex>)` in table `_bench_meta(key text PRIMARY KEY,
  value text)` inside each `jed_bench_<dataset>` database.

`bench-setup` (run via `rake bench:setup`) skips any engine/dataset pair whose stored
fingerprint matches; `--force` regenerates unconditionally. **Every harness binary
verifies the fingerprint before running** and aborts with `stale benchmark data: run
'rake bench:setup'` on mismatch or absence — a benchmark can never silently run against
wrong data.

## 6. Harness contract

Every benchmark binary takes the same positional arguments:

```
bench-<engine> <corpus_dir> <data_dir> <out_path> [name_filter_substring]
bench-setup    <corpus_dir> <data_dir> [--engine jed|sqlite|pg|all] [--force]
```

PG binaries use the standard `PG*` environment (the devcontainer points it at the Unix
socket). Human-readable progress goes to stderr; results go to `out_path` as JSONL,
truncated on open. One JSON object (single line, keys in this order) per completed
benchmark:

```json
{"schema":1,"bench":"point_lookup_pk","dataset":"large","engine":"jed","lang":"go",
 "variant":"core","iterations":50000,"warmup":2000,"total_ns":312000000,"ns_per_op":6240,
 "min_ns":4100,"p50_ns":5900,"rows_total":50000,"checksum":"9f86d081884c7d65",
 "fingerprint":"<sha256 hex>","started_at":"2026-06-12T14:03:11Z"}
```

- `engine` ∈ `jed | postgres | sqlite`; `lang` ∈ `go | rust | ts`; `variant` names the
  driver: `core` (jed), `pgx`, `postgres-crate`, `porsager`, `modernc`, `mattn-cgo`,
  `rusqlite`, `node-sqlite`. The comparison key is `(engine, lang, variant)`.
- Timing: per-iteration elapsed via the language's monotonic clock (Go `time.Now`, Rust
  `Instant`, TS `process.hrtime.bigint`); `ns_per_op = total_ns / iterations` (integer
  division), `min_ns`, `p50_ns` (sorted, lower median). Mean + min + p50, nothing more.
- `rows_total`: rows returned across measured iterations (0 for write kinds — their
  verification lives in the checksum).

### The answer checksum

**FNV-1a 64-bit** (offset basis `0xcbf29ce484222325`, prime `0x100000001b3`), folded over
the **measured** iterations only, emitted as 16 lowercase hex chars:

- per result value: hash the canonical rendering's UTF-8 bytes, then one `0x1F` byte;
- after each row: one `0x1E` byte;
- canonical rendering: NULL → `NULL`, integers → decimal string, text → its raw bytes.

For write kinds the checksum is the hash of the post-run sanity `count(*)` rendered the
same way (one value, one row) — `insert_rollback` proves the rollbacks held,
`insert_commit_durable` proves every commit landed.

Identical checksums across all binaries simultaneously prove: the PRNG ports agree (same
param sequences), the engines agree (same answers), and write semantics agree (same
post-state). A mismatch fails the report.

Pinned vector, asserted by a unit test in every harness alongside the PRNG vectors:
folding the two rows `(1, NULL, 'abc')` and `(-7)` yields `dd6e60407d30d28b` (generated
by the independent Ruby reference, like the PRNG vectors).

## 7. Engines, variants, dependencies

| Module | Dependency | Why |
|---|---|---|
| `bench/go` | `jed` via `replace ../../impl/go` | system under test |
| | `github.com/jackc/pgx/v5` | native Go PG driver; `$N` native; `CopyFrom` for the 1M-row load |
| | `modernc.org/sqlite` | pure-Go SQLite — the no-cgo comparison point |
| | `github.com/mattn/go-sqlite3` | cgo SQLite — the C-speed baseline; **cgo confined to this module** |
| | `github.com/BurntSushi/toml` | corpus parsing |
| `bench/rust` | `jed = { path = "../../impl/rust" }` | system under test |
| | `postgres` (sync) | PG client; `$N` native |
| | `rusqlite` (`bundled`) | SQLite, self-contained build |
| | `toml` | corpus parsing (same crate family as the core's dev-dep) |
| `bench/ts` | jed via relative import of `impl/ts/src/lib.ts` | system under test |
| | `postgres` (porsager) | PG client; raw `$N` via `sql.unsafe` |
| | `node:sqlite` | built-in (Node ≥ 22), zero dep |
| | `smol-toml` | corpus parsing (Node has no built-in TOML) |

These are **harness dependencies, not engine dependencies** (CLAUDE.md §14): the bench
modules are separate packages and the cores' manifests are untouched. The Go core's
pure-Go/no-cgo rule binds the *core*; `bench-sqlite-cgo` exists precisely to get the
C-SQLite baseline and its cgo never leaks past `bench/go`. New bench dependencies still
require explicit human confirmation, like any dependency.

## 8. Write benchmarks and the scratch database

Two write kinds:

- **`write_rollback`** — per iteration: open a transaction, run `batch` bound inserts,
  roll back (jed `begin(writable)` … `rollback()`; PG/SQLite `BEGIN` … `ROLLBACK`).
  Measures executor/binding throughput without growing the database. The post-run sanity
  `count(*)` must equal the dataset's committed row count.
- **`write_durable`** — per iteration: one statement as its own durable commit — the full
  fsync path. jed: autocommit (`synchronous=on` is the only mode today); PostgreSQL:
  default `synchronous_commit=on`; SQLite: `PRAGMA journal_mode=DELETE; PRAGMA
  synchronous=FULL`. The final `count(*)` must equal `warmup + iterations`.

`dataset = "scratch"` is reserved for `write_durable`: jed/SQLite harnesses create a
fresh file in a per-run temp dir under `bench/data/` (removed on exit); for PostgreSQL,
`bench-setup` creates an empty `jed_bench_scratch` database once and the harness runs
`DROP TABLE IF EXISTS` + the bench's `setup_sql` per run. No fingerprint applies to
scratch.

**Durability caveats.** On devcontainer filesystems (overlayfs / virtio volumes) fsync
can be artificially cheap or unevenly costly, and PostgreSQL's fsync happens server-side
on the PG service's own volume while jed/SQLite fsync the local one — durable-commit
numbers are indicative, compare with care. There is also a standing client/server
asymmetry: every PG number includes IPC over the Unix socket, which is PG's deployment
model but not jed's or SQLite's.

## 9. Running and reporting

```
rake bench:setup        # build + run bench-setup (fingerprint-gated; [force] to override)
rake bench:run          # build all binaries, run them sequentially, results to
                        #   bench/results/<UTC-stamp>/, then report + HTML
rake "bench:run[point_lookup]"   # substring filter, passed through to every binary
rake bench:report       # re-aggregate the newest (or a given) results dir
rake bench:html         # static HTML report for the newest (or a given) run dir,
                        #   diffed against the previous run by default
rake bench:markdown     # the same report as Markdown, to stdout + <dir>/report.md
rake "bench:diff[a,b]"  # machine-readable JSONL diff of two runs (default: newest
                        #   vs previous)
```

Three reporters share one loader/verifier (`scripts/bench_results.rb`); each **exits 1**
if any two results in a run disagree on `checksum` (wrong answer somewhere — treat it
like a failing conformance test) or on `fingerprint` (mixed-vintage data):

- `scripts/bench_report.rb` — the terminal matrix: groups results by `(bench, dataset)`
  and prints fixed-width `ns_per_op` (humanized ns/µs/ms) with one column per
  `engine/lang/variant`; `-v` adds min/p50.
- `scripts/bench_html.rb [run] [baseline] [--no-baseline]` — writes a self-contained
  `<run_dir>/report.html` (stdlib ERB, inline CSS, zero JS): per-benchmark bar charts
  sorted fastest-first, multipliers vs the fastest, min/p50 tooltips, and — against the
  baseline run (default: the one before it) — per-pair Δ% colored with a 5% noise floor.
  On verification failure the page is still written, failures in a red banner. A
  baseline with a *different* fingerprint is a warning in the page (the runs measured
  different data), not a failure.
- `scripts/bench_markdown.rb [run] [baseline] [--no-baseline]` — the same report (the
  two renderers share `BenchResults.report_model`, so they cannot drift) as Markdown:
  printed to stdout for reading at the terminal and written to `<run_dir>/report.md`
  for the VS Code markdown preview. GFM tables with block-character bars; cells are
  space-padded so the raw text aligns at the terminal. Same defaults, failure handling,
  and fingerprint warning as the HTML report.
- `scripts/bench_diff.rb [run] [baseline] [--json] [--fail-over=PCT]` — the machine
  surface (built for tooling and AI agents; this is the one-command form of the
  CLAUDE.md §10 before/after obligation). Emits JSONL: one object per joined
  `(bench, dataset, engine, lang, variant)` with `before_ns_per_op`/`after_ns_per_op`/
  `delta_pct`/`checksum_match`, with `before_only`/`after_only` flags making
  partial/filtered runs explicit, plus a trailing `{"summary":…}` line (fingerprints,
  improved/regressed/noise counts at the same 5% floor). `--json` emits one pretty
  document instead; `--fail-over=PCT` exits 2 if any matched pair regressed by more
  than PCT% — an operator-side regression gate, never part of `rake ci`.

`rake ci` does **not** run benchmarks, and never will (§1).

## 10. Methodology caveats

Single-threaded, one binary at a time, no process pinning in v1 — run on an otherwise
idle machine; `taskset`/`nice` are at the operator's discretion. Wall-clock numbers are
relative to a machine and a moment: compare *within* a run (and trends across runs on
the same box), not absolute values across machines. Warmup iterations exist to populate
caches (jed's buffer pool, PG's shared buffers, SQLite's page cache) and JIT-warm the TS
core; they consume the same param stream so the measured window is identically
distributed across engines.

## 11. Backfill and the growth obligation

v1 ships six benchmarks (point lookup, secondary-index lookup, full-scan aggregate,
ORDER BY + LIMIT, insert+rollback throughput, durable single-row commits) over two
datasets (10k / 1M rows). Known gaps, tracked in TODO.md Phase 8:

- a join benchmark (needs a second dataset table → `generator_version` bump);
- GROUP BY aggregate; UPDATE / DELETE throughput; miss-heavy point lookups;
- text-heavy / large-value rows (exercise the overflow + LZ4 path);
- `SharedDb` concurrent-reader throughput (once file-backed); cold-open time;
- durable-commit batch-size sweep (1 vs 100 vs 1000 rows per commit).

**Standing obligation** (CLAUDE.md §10): a perf-relevant feature lands with a benchmark
the same way an optimization lands with a NoREC relation; a perf-sensitive change runs
the affected benchmarks before and after, and both numbers go in the change description.
