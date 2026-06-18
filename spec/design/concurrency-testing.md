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

## 3. Three layers

| Layer | Tests | Deterministic? | In `rake ci`? | Single-thread core (TS) |
|---|---|---|---|---|
| **1. Schedule** | snapshot isolation, cross-handle visibility, the watermark | yes — explicit total order | yes (conformance) | runs identically (sequential) |
| **2. Await** | write-gate blocking | yes — equivalent serial order | yes | modeled (no real block) |
| **3. Stress** | races, commit atomicity, reclamation-under-load | no — random schedule | no (bench-family) | seeded sequential interleave, or skip |

Layers 1–2 join the differential contract. Layer 3 belongs to the benchmarks family
([benchmarks.md](benchmarks.md)): nondeterministic schedule, deterministic *checked answer*.
**Layer 1 has landed on all three cores** (stepped-sequential everywhere; the stepped-threaded mode
on Go and Rust); **2 and 3 are specified here as follow-ons.**

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

### 4.1 Directives

Operations are scoped to a named **session**; control and assertion directives are new.

```
open  <sid> read | write     # read → db.read() pins the committed version; write → db.write() takes the gate
on    <sid> <record>          # run a sqllogictest record (statement|query) against session <sid>
commit   <sid>                # WriteHandle.Commit — publish + release the gate
rollback <sid>                # WriteHandle.Rollback — discard + release the gate
close    <sid>                # ReadHandle.Close — deregister, advancing the watermark
expect version     <n>        # SharedDB.Version() — the published committed version
expect oldest_live <n>        # SharedDB.OldestLiveTxid() — the reclamation watermark
```

The text after `on <sid>` is parsed by the existing record parser. A `read` session may only
read (a write is `25006`, never poisoning the handle); a `write` session must end with
`commit`/`rollback`; a `read` session ends with `close`.

### 4.2 Example (the canonical scenario, today only in the three per-core suites)

```
# format: concurrency
# requires: txn.shared, txn.read_handle, txn.watermark, ddl.create_table, ddl.primary_key, dml.insert, query.select, query.order_by, types.int32

open w write
on w statement ok
CREATE TABLE t (id int32 PRIMARY KEY)
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
  (`go test -race`; Rust's `Send`/`Sync`, proven by moving `SharedDb` into each worker, + the
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

## 5. Layer 2 — the write-gate `await` extension (follow-on)

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

Forward-compatible: if the model ever grows row-level locks or multiple writers, `blocks` /
release generalizes, and a release that never arrives becomes a deadlock assertion. Gated by
a `txn.gate_blocking` capability (not yet defined — added with the first Layer-2 file).

## 6. Layer 3 — the parallelism stress format (follow-on)

Non-deterministic schedule, real threads on Rust/Go, **invariants** instead of exact
transcripts. Belongs to the benchmarks family ([benchmarks.md](benchmarks.md)): outside `rake
ci`, but its answers are still checked. TOML fits its programmatic shape:

```toml
[meta]
name        = "balance-transfer-sum-invariant"
description = "Concurrent transfers keep total balance constant on every snapshot"
parallel    = "optional"   # optional → seeded-sequential fallback on single-thread cores; required → skip
seed        = 1234         # drives the fallback scheduler and any workload randomness

[setup]                    # run once, deterministically
sql = [
  "CREATE TABLE acct (id int32 PRIMARY KEY, bal int64)",
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

Three checks, increasing in strength:

1. **Per-snapshot invariant** — an answer known by construction (SQLancer-style, no oracle).
   `sum(bal) == 1000` on every reader snapshot catches torn reads and isolation violations —
   including the scariest one, a page reclaimed while a reader still pins it (the watermark
   bug), which would surface here as a wrong sum.
2. **Confluent final state** — when the author designs the workload so the result is
   order-independent (disjoint-id inserts, uniform increments), assert exact final rows *and*
   cross-core byte-identity via the file-format checksum (the §10 benchmarks mechanism).
   Non-confluent workloads drop to invariant-only.
3. **In-process** — Rust/Go run it under the race detector; the harness flags any worker that
   deadlocks (timeout) or any commit-count mismatch (lost update).

**Fallback** (`parallel = "optional"`): a single-threaded core runs the same worker
definitions through a **seeded pseudo-random sequential interleaver** (deterministic given
`seed`) — reproducing the logical interleavings (so it still catches isolation/atomicity/
visibility bugs) while honestly not exercising CPU parallelism or memory races (which a
single-threaded Worker cannot have). `parallel = "required"` skips on such cores, reported as
skipped (no silent cap — CLAUDE.md §8).

Layer 3's real payoff arrives once **file-backed sharing** lands (transactions.md §8 / the
shared-handle persist wiring): today, in-memory, the free-list-reuse-vs-live-reader path does
not run, and that contended path is exactly where a stress harness earns its keep.

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

- `spec/conformance/suites/concurrency/*.test` — Layer 1 (and later 2). New capabilities in
  `manifest.toml`: `txn.shared`, `txn.read_handle`, `txn.watermark` (and later
  `txn.gate_blocking`). Validated by `rake verify`; **in `rake ci`** for any core declaring
  them.
- `stress/*.stress.toml` + a `rake stress` task — Layer 3; **outside `rake ci`**, registered
  alongside the determinism ledger as timing-nondeterministic (like `bench/`).

### Capabilities (Layer 1)

| id | meaning |
|---|---|
| `txn.shared` | the goroutine/thread-safe `SharedDb` handle — concurrent readers + a single writer (transactions.md §10) |
| `txn.read_handle` | a `ReadHandle` pins a consistent committed snapshot for its life; a write through it is `25006`; `close` deregisters it |
| `txn.watermark` | `oldest_live_txid` tracks the minimum version any live reader pins (the Phase-6 reclamation gate, transactions.md §8) |

## 9. Status

- **Layer 1 — landed (all three cores).** The format, the `concurrency/` suite, the `# format:
  concurrency` dispatch, and the three capabilities. **All three cores run the schedule
  stepped-sequentially** inside their conformance harness (`impl/{go,rust,ts}` — the binary's
  default; this *defines* the canonical, timing-free result). **Go and Rust additionally run the
  stepped-threaded mode** — one goroutine/OS-thread per session under a turn token — driven under
  the race detector by `rake concurrency:race` (`go test -race`; Rust `cargo test` proving
  `Send`/`Sync` + the threaded run, a TSan run optional). TS is sequential-only (JS has no
  shared-memory threads for live objects, §4.3). Two schedules so far: `snapshot_isolation.test`
  (cross-handle visibility + the watermark) and `watermark_refcount.test` (reader refcounting + a
  rolled-back writer).
- **Layer 2 — specified (§5), not built.** Lands with the first `txn.gate_blocking` file.
- **Layer 3 — specified (§6), not built.** Lands as `stress/` + `rake stress`, most valuable
  once file-backed sharing is wired.
