# Concurrency & parallelism testing — design

> How the engine's concurrency model (CLAUDE.md §3, [transactions.md](transactions.md)
> §8/§10) is brought *inside* the cross-core conformance contract. The sqllogictest corpus
> (§7) is single-handle and sequential, so until now concurrency was tested only by
> hand-mirrored per-core suites (`impl/go/shared_test.go`, `impl/rust/tests/shared.rs`,
> `impl/ts/tests/shared.test.ts`) — coverage that can silently drift. This doc defines a
> language-neutral concurrency script format so the concurrency contract is authored once
> and verified everywhere, the same way [conformance.md](conformance.md) handles the
> sequential surface. When a decision here changes, update [conformance.md](conformance.md)
> and CLAUDE.md §7 in the same edit.

## 1. The gap this closes

CLAUDE.md §2's honesty mechanism is *divergence under a shared contract*: every spec
ambiguity becomes a failing test the day it is written, enforced by the **shared** corpus.
The sqllogictest format the corpus uses (`statement ok`, `query …`, run in order against
**one** `Database` handle — [conformance.md](conformance.md) §1) structurally cannot express
two transactions interleaving, a reader pinned while a writer commits, the reader-liveness
watermark, or write-gate blocking. So the concurrency model — the part of the engine that is
*hardest* to get right and most divergence-prone across a `Send`/`Sync` Rust core, a
goroutine Go core, and a single-threaded TS core — sits **outside** the differential net,
checked only by three per-core suites that mirror each other by hand. A divergence in, say,
when the watermark advances would not be caught by the corpus; only a human noticing the
mirror suites drifted would catch it.

This doc pulls concurrency back inside the net.

## 2. The enabling property: results depend only on logical order

The reason this is tractable — and the reason the format can be **deterministic** without
real threads — is specific to jed's model (transactions.md §2/§5):

- a read transaction sees an immutable snapshot pinned at `read()` and is never disturbed by
  any later commit;
- there is at most one writer, and a writer publishes atomically;
- so the **observable result of every operation is a pure function of the logical order of
  commits and the version each reader pinned** — never of wall-clock timing, thread
  scheduling, or interleaving *within* an uncontended step.

Therefore a *deterministic* concurrency test does not need threads at all. It needs an
**explicit schedule**: a total order over multi-session operations. Executing that order on a
single thread yields the canonical result, and that result is exactly what any real
multi-threaded run consistent with the order must also produce. True CPU parallelism is a
*separate*, non-deterministic concern handled by a stress layer that checks invariants rather
than exact transcripts.

## 3. Four layers

| Layer | Tests | Deterministic? | In `rake ci`? | Single-thread core (TS) |
|---|---|---|---|---|
| **1. Schedule** | snapshot isolation, cross-handle visibility, the watermark | yes — explicit total order | yes (conformance) | runs identically (sequential) |
| **2. Await** | write-gate blocking | yes — equivalent serial order | yes | modeled (no real block) |
| **3. Stress** | races, commit atomicity, reclamation-under-load | no — random schedule | no (bench-family) | seeded sequential interleave, or skip |
| **4. Process** | shared-file OS protocol, cross-core interoperability, crash release | barrier-directed; timing outcomes bounded | yes once `file.shared_process` lands | Node participates through the native adapter |

Layers 1–2 join the differential contract. Layer 3 belongs to the benchmarks family
([benchmarks.md](benchmarks.md)): nondeterministic schedule, deterministic *checked answer*.
**The first three layers have landed.** Layers 1–2 run on all three cores (stepped-sequential everywhere;
the stepped-threaded mode on Go and Rust). **Layer 3** (§6) lands as `stress/*.stress.toml` + `rake
stress`, **outside `rake ci`** (bench-family), with real-threads workers on Go (under the race
detector) and Rust and a seeded-sequential interleaver on TS. **Layer 4** (§10) is the required,
not-yet-built real-process contract for [locking.md](locking.md).

## 4. Layer 1 — the deterministic schedule format

A suite under `spec/conformance/suites/concurrency/`. Files use the `.test` extension and the
existing `# requires:` gate, plus one new header that routes them to the concurrency runner:

```
# format: concurrency
```

A file without that header is an ordinary sqllogictest file. The runner reuses the corpus
result grammar verbatim (`statement ok` / `statement error <code>`, `query <coltypes>
<sortmode>` + `----` + expected rows, with `rowsort`/`valuesort` and the `R` float tag), so
all row rendering, sorting and comparison code is shared with the sequential harness.

A concurrency file may also carry the file-level **`# attach: <name>`** directive (the same one
the sequential harness reads — [attached-databases.md](attached-databases.md) §6): before the
schedule runs, the runner attaches a fresh empty read-write in-memory database under `<name>` to the
shared handle. Attaching is a host-API act, never SQL (attached-databases.md §2), so it happens when
the shared `Database` is built — not as a schedule step — and, because attachments are
Database-scoped, every session the schedule opens sees them. This is what lets a schedule exercise
concurrent readers and a writer over an *attached* database: a reader pins the whole roots — main
plus the attachment snapshots — in one lock-free `Load`, so cross-database snapshot isolation and the
watermark are asserted over an attachment exactly as over main (Slice 1b-3). Gated by
`harness.attach` + `attach.in_memory`, so a core that has not wired the directive into its
concurrency runner skips the file before parsing (§4.4).

### 4.1 Directives

Operations are scoped to a named **session**; control and assertion directives are new.

```
open  <sid> read | write     # read → db.read_session() pins the committed version; write → db.write_session()
on    <sid> <record>          # run a sqllogictest record (statement|query) against session <sid>
commit   <sid>                # session.commit() — publish + release the gate
rollback <sid>                # session.rollback() — discard + release the gate
close    <sid>                # session.close() — deregister, advancing the watermark
expect version     <n>        # db.version() — the published committed version
expect oldest_live <n>        # db.oldest_live_txid() — the reclamation watermark
```

The text after `on <sid>` is parsed by the existing record parser. A `read` session may only
read (a write is `25006`, never poisoning the handle); a `write` session must end with
`commit`/`rollback`; a `read` session ends with `close`.

### 4.2 Example (the canonical scenario, today only in the three per-core suites)

```
# format: concurrency
# requires: txn.shared, txn.read_handle, txn.watermark, ddl.create_table, ddl.primary_key, dml.insert, query.select, query.order_by, types.i32

open w write
on w statement ok
CREATE TABLE t (id i32 PRIMARY KEY)
on w statement ok
INSERT INTO t VALUES (1)
commit w
expect version 1

open r read                 # pins version 1 (one row)
expect oldest_live 1

open w2 write
on w2 statement ok
INSERT INTO t VALUES (2)
commit w2
expect version 2
expect oldest_live 1        # r still pins v1 → watermark held at 1

on r query I rowsort        # the pinned reader still sees only its snapshot
SELECT id FROM t
----
1

open r2 read                # a fresh reader sees both rows
on r2 query I rowsort
SELECT id FROM t
----
1
2

close r
expect oldest_live 2        # r gone → watermark advances
close r2
```

Every line of output is determined by the listed order. The `expect version` / `expect
oldest_live` directives promote the watermark — currently asserted only in per-core code —
to a cross-core contract.

### 4.3 Execution modes

The same file runs two ways:

- **stepped-sequential** (default, *every* core including single-threaded TS): walk the steps
  in order on one thread. This **defines** the canonical output.
- **stepped-threaded** (opt-in, Rust/Go — **landed**, run by `rake concurrency:race`): give each
  session its own thread/goroutine and enforce the listed order with a turn token (the driver sends
  a step to its session, waits for the reply — and, for an end step, joins the thread — then
  advances; each session creates and ends its handle on its own thread). Same schedule, same
  deterministic result — but it drives the **real concurrent code paths under the race detector**
  (`go test -race`; Rust's `Send`/`Sync`, proven by moving the `Database` into each worker, + the
  threaded run, with a TSan run optional), catching memory races that sequential stepping cannot.

Layer 1 thus yields a deterministic cross-core result *and*, on the threaded cores, optional
race-detector coverage of the actual concurrency implementation.

### 4.4 Cross-core gating

A concurrency file `requires` `txn.shared` (and friends). A core that has not implemented the
shared handle does not declare those capabilities, so its harness **skips** the file before
parsing any record — the standard capability gate ([conformance.md](conformance.md) §3),
verified to skip-before-parse in all three harnesses. This is what let Layer 1 land Go-first
without breaking Rust/TS: each skipped until its runner existed. All three cores now declare these
capabilities and run the schedule, so none skip today; the gate remains the mechanism by which any
*future* core (or a core mid-port) skips — before parsing — until its runner exists. No silent pass
— a skip is always reported.

## 5. Layer 2 — the write-gate `await` extension (landed)

The only blocking operation in the current model is `open <sid> write` while another writer
holds the gate. One annotation expresses it:

```
open w2 write blocks          # asserts the gate is currently held
...
commit w1                     # the releasing step — w2's open now logically completes
on w2 statement ok
INSERT INTO t VALUES (9)
commit w2
```

**Canonical semantics:** `blocks` denotes the *equivalent serial order* — `w2` opens
immediately after the gate-releasing step. Results stay deterministic and identical on every
core (a single-threaded core simply queues `w2` until release; it never needs to truly
block). A **threaded** harness MAY additionally verify real blocking (spawn the open, confirm
it has not returned before the releasing step, then join) as a stronger check — confining all
timing-sensitivity to that optional verification while the *result* contract stays timing-free.

**How it lands.** A `blocks` open is **not run when it is seen** — calling `write()` then would
block the runner forever on the held gate (Go/Rust), or throw `25001` (the TS core, which cannot
block one thread). It is **recorded and run at the gate-releasing step**, the instant the holder
commits/rolls back — so on every core the deferred `write()` finally executes against a *free* gate
and succeeds uniformly, capturing the version the holder just published (read-your-writes across the
hand-off). The single-writer model bounds this to **at most one blocked writer at a time** (a second
`blocks` is a schedule error). The **stepped-threaded** cores (Go/Rust) do *not* defer: the queued
writer's thread parks inside the real `write()` on the held gate (its open ack withheld) until the
holder releases it — driving the actual blocking acquire/condvar-wakeup under the race detector (the
one concurrency path the sequential walk never exercises), and the driver verifies the open had **not**
returned before the release (the optional stronger check above). First file:
`suites/concurrency/gate_blocking.test`.

Forward-compatible: if the model ever grows row-level locks or multiple writers, `blocks` /
release generalizes (the one-blocked-writer bound lifts), and a release that never arrives becomes a
deadlock assertion. Gated by the `txn.gate_blocking` capability.

## 6. Layer 3 — the parallelism stress format (landed)

Non-deterministic schedule, real threads on Rust/Go, **invariants** instead of exact
transcripts. Belongs to the benchmarks family ([benchmarks.md](benchmarks.md)): **outside `rake
ci`** (registered as timing-nondeterministic, like `bench/` — §8), but its answers are still
checked, loudly. TOML fits its programmatic shape; files live in `stress/*.stress.toml`:

```toml
[meta]
name        = "balance-transfer-sum-invariant"
description = "Concurrent transfers keep total balance constant on every snapshot"
parallel    = "optional"   # optional → seeded-sequential fallback on single-thread cores; required → skip
seed        = 1234         # drives the fallback scheduler and any workload randomness

[setup]                    # run once, deterministically (whole-image autocommit, before any worker)
sql = [
  "CREATE TABLE acct (id i32 PRIMARY KEY, bal i64)",
  "INSERT INTO acct VALUES (1, 1000), (2, 0)",
]

[[worker]]                 # writers contend on the single-writer gate
kind = "writer"
count = 4
iterations = 250
op = "BEGIN; UPDATE acct SET bal = bal - 1 WHERE id = 1; UPDATE acct SET bal = bal + 1 WHERE id = 2; COMMIT;"

[[worker]]                 # readers assert an invariant on every snapshot
kind = "reader"
count = 8
iterations = 500
invariant_query  = "SELECT sum(bal) FROM acct"
invariant_expect = "1000"   # must hold on every snapshot — proves commit atomicity across handles

[final]                    # confluent workload → schedule-independent final state
query = "SELECT id, bal FROM acct ORDER BY id"
expect = [[1, 0], [2, 1000]]
cross_core_checksum = true  # final committed image byte-identical across cores
```

A **writer** worker runs `iterations` transactions; each takes the single-writer gate
(`db.write_session()` — which **blocks** on a held gate in Go/Rust, real contention), runs `op`,
and commits. `op`'s `BEGIN`/`COMMIT` are the transaction *markers*: the runner maps them onto the
session's open/commit (it strips bare `BEGIN`/`COMMIT`/`ROLLBACK` and runs the inner statements in
the one write transaction). A **reader** worker runs `iterations` snapshots; each opens a read-only
session (`db.read_session()` — never blocks), runs `invariant_query`, asserts it renders
`invariant_expect`, and closes (advancing the watermark). Both kinds render integer columns
(the value/checksum surface Layer 3 needs today; the bench FNV-1a answer checksum, benchmarks.md
§6, folds them).

Three checks, increasing in strength:

1. **Per-snapshot invariant** — an answer known by construction (SQLancer-style, no oracle).
   `sum(bal) == 1000` on every reader snapshot catches torn reads and isolation violations —
   including the scariest one, a page reclaimed while a reader still pins it (the watermark
   bug), which would surface here as a wrong sum.
2. **Confluent final state** — when the author designs the workload so the result is
   order-independent (disjoint-id inserts, uniform increments), assert exact final rows *and*
   cross-core byte-identity via the answer checksum (the §10 benchmarks mechanism). Because the
   workload is confluent, **every core agrees on the final checksum regardless of mode** (real
   threads or the seeded interleaver) — `rake stress` cross-checks it and fails on any
   disagreement. The exact `[final].expect` rows also encode the **lost-update** check: 1000
   transfers must have moved 1000 from acct 1 to acct 2. Non-confluent workloads drop to
   invariant-only (`final.expect` omitted).
3. **In-process under the race detector** — Go runs it under `-race` (one goroutine per worker
   over the shared handle), Rust over real OS threads (proving `Send`/`Sync`; TSan optional);
   the harness flags any worker that deadlocks via a per-run **timeout watchdog**.

### Execution modes

Each core runs a stress file one of two ways, both driving the *same* worker definitions:

- **threaded** (Go/Rust, `parallel != "skip"`) — spawn `count` real threads/goroutines per
  worker block, all running concurrently; writers contend on the gate for real, readers pin
  snapshots for real. The OS schedules the interleaving; the seed is unused (a fixed-`op`
  workload has no per-iteration randomness yet). A watchdog times the whole fan-out out.
- **seeded-sequential** (TS always; any core, on request) — a **deterministic interleaver**: the
  workers are flattened to a fixed index order, each modeled as a program of atomic ops (writer:
  `acquire · exec… · commit`; reader: `open · check · close`), and at each step the
  shared splitmix64(`seed`) stream picks the next *runnable* worker (an `acquire` is runnable only
  while the gate is free — single-writer — so the interleaver never needs to block, the property
  that lets the single-threaded TS core run it at all). It reproduces the logical interleavings
  (so it still catches isolation/atomicity/visibility bugs) while honestly **not** exercising CPU
  parallelism or memory races (which a single-threaded Worker cannot have). It is deterministic
  given `seed`, so a sequential run is reproducible. `parallel = "required"` **skips** on a
  single-thread core, reported as skipped (no silent cap — CLAUDE.md §8).

### Where it lives

Layer 3 is bench-family, so its runner reuses the bench modules' machinery rather than the
conformance harness: one stress binary per core (`bench/go/cmd/stress`,
`bench/rust/src/bin/stress.rs`, `bench/ts/src/stress.ts`) over the shared splitmix64 PRNG, the
FNV-1a answer checksum, and the TOML parser those modules already carry (no new dependency, no
new module, no core-manifest change — benchmarks.md §7). Each binary parses `stress/*.stress.toml`,
runs every file in its native mode, and emits one JSONL result line per file (name, lang, mode,
status, invariant-check count, final-ok, checksum). `rake stress` builds + runs all three
(Go under `-race`) and aggregates: any `fail` fails the task, and for a `cross_core_checksum`
file the passing cores' checksums must all match.

Layer 3's real payoff arrives once **file-backed sharing** lands (transactions.md §8 / the
shared-handle persist wiring): today, in-memory, the free-list-reuse-vs-live-reader path does
not run, and that contended path is exactly where a stress harness earns its keep. Until then it
already exercises commit atomicity across handles, snapshot isolation under contention, and the
watermark — the in-memory subset — across the real concurrent code paths.

## 7. The per-core harness interface

Both formats drive one small driver per core — a 1:1 mapping onto each core's shared handle
(`shared.go` / `shared.rs` / the TS core), consistent with §6's "thin harness":

```
open(sid, mode)            # read pins committed / write takes the gate
exec(sid, sql)  -> ok | error(code)
query(sid, sql) -> rows    # compared via the sqllogictest rules
commit(sid) / rollback(sid) / close(sid)
version()       -> int
oldest_live()   -> int
# threaded mode adds a turn-token barrier (L1); stress adds worker spawn (L3)
```

Everything else is shared, language-neutral data — which is the point: Layers 1–2 live inside
the "spec is the contract" net, Layer 3 inside the "checked-answer benchmarks" net.

## 8. Placement, capabilities, CI

- `spec/conformance/suites/concurrency/*.test` — Layers 1 and 2. Capabilities in
  `manifest.toml`: `txn.shared`, `txn.read_handle`, `txn.watermark`, `txn.gate_blocking`.
  Validated by `rake verify`; **in `rake ci`** for any core declaring them.
- `stress/*.stress.toml` + a `rake stress` task — Layer 3; **outside `rake ci`**, registered
  alongside the determinism ledger as timing-nondeterministic (like `bench/`).
- `spec/conformance/process/*.process.toml` + `rake concurrency:process` — Layer 4; one shared
  language-neutral scenario is driven against same-core and Rust↔Go↔Node actor combinations. It enters
  `rake ci` when `file.shared_process` lands; Windows/macOS run the same corpus in their platform lanes.

### Capabilities (Layers 1–2)

| id | meaning |
|---|---|
| `txn.shared` | the goroutine/thread-safe `Database` core — concurrent reader/writer **sessions** (transactions.md §10; flag name retained from the pre-convergence `SharedDb`) |
| `txn.read_handle` | a read-only session pins a consistent committed snapshot for its life; a write through it is `25006`; `close` deregisters it (flag name retained from the pre-convergence `ReadHandle`) |
| `txn.watermark` | `oldest_live_txid` tracks the minimum version any live reader pins (the Phase-6 reclamation gate, transactions.md §8) |
| `txn.gate_blocking` | the Layer 2 `open <sid> write blocks` annotation — a writer-open on the held single-writer gate, queued until the holder releases it (the equivalent serial order, §5) |

## 9. Status

- **Layer 1 — landed (all three cores).** The format, the `concurrency/` suite, the `# format:
  concurrency` dispatch, and the three capabilities. **All three cores run the schedule
  stepped-sequentially** inside their conformance harness (`impl/{go,rust,ts}` — the binary's
  default; this *defines* the canonical, timing-free result). **Go and Rust additionally run the
  stepped-threaded mode** — one goroutine/OS-thread per session under a turn token — driven under
  the race detector by `rake concurrency:race` (`go test -race`; Rust `cargo test` proving
  `Send`/`Sync` + the threaded run, a TSan run optional). TS is sequential-only (JS has no
  shared-memory threads for live objects, §4.3). Four schedules so far: `snapshot_isolation.test`
  (cross-handle visibility + the watermark), `watermark_refcount.test` (reader refcounting + a
  rolled-back writer), `gate_blocking.test` (the Layer 2 write-gate block + hand-off), and
  `attach_snapshot_isolation.test` (cross-database snapshot isolation + the watermark over a
  host-attached in-memory database via the `# attach:` directive, §4 — Slice 1b-3, which pulled
  attachments inside this net; the threaded cores drive a reader pinning `roots.attached` on its own
  goroutine/thread while a writer publishes a fresh attached map, under the race detector).
- **Layer 2 — landed (all three cores).** The `open <sid> write blocks` annotation and the
  `txn.gate_blocking` capability (§5). All three cores defer the queued open to the gate-releasing
  step (the canonical, timing-free result); Go and Rust additionally park the queued writer's thread
  inside the real `write()` on the held gate under the race detector (`rake concurrency:race`),
  verifying the open had not returned before the release. First file: `gate_blocking.test`.
- **Layer 3 — landed (bench-family, outside `rake ci`).** The `stress/*.stress.toml` format, a
  stress binary per core (`bench/{go,rust,ts}` — reusing the bench splitmix64 PRNG + FNV-1a answer
  checksum + TOML parser, §6), and the `rake stress` task that cross-checks the confluent final
  checksum across cores. Go runs under `-race` (one goroutine per worker), Rust over real OS
  threads, TS via the seeded-sequential interleaver. First file:
  `stress/balance_transfer.stress.toml` (the balance-transfer sum invariant + confluent final
  state). Most valuable once file-backed sharing is wired (§6), but already exercises commit
  atomicity, snapshot isolation under contention, and the watermark over the real concurrent paths.
- **Layer 4 — landed.** It is release-blocking for shared file access and
  owns the `file.shared_process` capability. The feature is not represented by three mirrored unit
  suites: the one process corpus below drives every supported core pairing over the same file.

## 10. Layer 4 — real-process shared-file protocol

Layer 4 exists because Layers 1–3 cannot exercise kernel lock lifetime, process death, or two native
cores interpreting one bundle. A `.process.toml` scenario declares named actors (`rust`, `go`, or
`node`), one temporary database path, and an explicit sequence of commands and barriers. Each core
supplies a small test-only actor binary that accepts create/open/begin/query/commit/rollback/close and
named fault-hook commands over stdin and returns framed results over stdout. The shared Ruby driver
owns scheduling, deadlines, process kill, and transcript comparison.

The corpus, not per-core copies, covers:

- first-rollout/unknown-marker failure, join, alone→shared→alone transitions, and open/writer timeout;
- Rust↔Go, Rust↔Node, Go↔Node, and same-core reader/writer handoff;
- a reader pinned before a foreign commit, append-only page allocation, meta refresh, and plan
  invalidation after foreign `txid`;
- kill after every lock acquisition, body sync, and meta-publication hook; the survivor must select a
  valid old/new meta and the kernel must release every dead actor's locks;
- compaction lock continuity, symlink identity, hard-link rejection, and bundle-entry replacement
  failure; and
- a confluent multi-writer checksum plus a file-format verifier pass after every crash scenario.

Timing is never an expected value. The driver uses barriers to establish happens-before facts and only
asserts “still blocked before release,” “completed before the documented deadline,” or the specified
SQLSTATE. No sleep establishes correctness. Platform lanes may use generous outer watchdogs, but the
protocol's own timeout uses each core's monotonic clock.
